---
status: plan
summary: dedup-bits (2026-10-29): one bit-set family in blaze_ids::bits (Bits over any word storage and index type, BitMatrix, Ranked, AtomicBits) replacing about 45 copies across 25 crates, with its speed of light.
date: 2026-10-29
---
# dedup-bits: bit sets

Part of [dedup](../dedup.md). The shared implementation is
[`crates/blaze-ids/src/bits.rs`](../../../../../crates/blaze-ids/src/bits.rs); its tests are
`crates/blaze-ids/src/bits/tests.rs`.

## The copies

The brief named 19; the search (`/ 64]`, `% 64)`, `>> 6`, `count_ones` loops, `Vec<u64>`
newtypes, `rustc_index` sets) found about 45, in five shapes:

| shape | copies |
|---|---|
| a `Vec<u64>` sized once, insert/contains/remove, some with union | diagnostics `dead/defs.rs`, `bodies/bits.rs`, `liveness/flow.rs`; hir-ty-mir `move_paths.rs`, `dse.rs`; borrowck `BitSet`; codegen `imports.rs`; cargo `unit/graph.rs`; proof `Bits`; vfs `Written`; hir-ty-codegen `refs.rs` `Blocks`; usercode `queue.rs` |
| a growing `Vec<u64>` (insert grows) | diagnostics `labels.rs`, `levels.rs`; hir-ty `IdxSet`, `VarSet`; blaze-query `Marks`; intern `Live`; link `IdSet`; hir-ty-ub `borrows.rs` |
| small inline storage (`SmallVec<[u64; 1]>`, `ThinVec`, first word inline) | diagnostics `imports/used.rs`; hir-def `VariantBits`, `item_scope.rs` `Bits`; hir-ty `ParamsInRepr` |
| fixed `[u64; W]`, some `const` | units `target.rs`; hir-ty `TargetFeatureSet`; diagnostics `LintSet`; parser `TokenSet`; inproc `fence.rs` |
| rows, ranks, atomics | borrowck `BitMatrix`, `rustc_index::BitMatrix` users; items `table.rs`, hir-ty `Presence`, syntax-tree `flat.rs` ranks; cas `flags.rs`, `table.rs`, image `checked`/`bad` |

Plus inline word arithmetic in a dozen functions (vfs `tree.rs`, link `commit.rs`,
`macho/dylib.rs`, span `ast_id.rs`, hir-def `import_map.rs`, cargo `job.rs`, stencil,
hir-ty-codegen `backend.rs`, borrowck `constraints.rs`, hir-ty-ub `memory.rs`).

Which callers moved and which copies stay on purpose, with why, is in the report
(`/tmp/blaze-lanes/v2-dedup-bits.md`) and its children's.

## What was picked

None of the copies covered more than a third of what callers do, and five storage shapes
can't share one concrete type without costing the small ones their size (`VariantBits` is 24
bytes inline by assertion, `ParamsInRepr` 16, `LintSet` 40). So the set is one generic type:

- **`Bits<S, I>`**: the operations written once over `S: Words` (read), `WordsMut` (write) or
  `GrowWords` (resize), implemented for `Vec<u64>`, `Box<[u64]>`, `[u64; W]`, `&[u64]`,
  `&mut [u64]`, `SmallVec<[u64; N]>` and `ThinVec<u64>`; `I: BitIndex` is the member type
  (`usize`, `u32`, `u16`, `u8`, or the caller's own, as rustc's `DenseBitSet<T: Idx>`).
  Aliases name the common shapes: `BitSet` (`Vec`), `BitArray<W>` (inline, with `const`
  operations for sets made at compile time), `BitSlice`/`BitSliceMut` (views: a matrix row, a
  mapped column).
- **`BitMatrix`**: rows of one width in one buffer, a row a multiply away; `union_rows`
  splits the buffer once so a row is read and written in one loop. Its buffer is a storage
  parameter after the index (`BitMatrix<I, S = Vec<u64>>`, index first because an owned
  matrix of a caller's index is what nearly every caller names), so borrowck's matrices live
  in a frame's words (`BitMatrix<I, &'f mut [u64]>`, `from_words`) with no global allocation.
- **`Ranked`**: a fixed set with a `u32` count of members before each word, so a member's
  rank (its place in a column of the members' values) is one popcount: rank9's idea with a
  word-granular count, 1.5 bits a bit. `ranks_of`/`rank_in` do the same for columns held
  elsewhere.
- **`AtomicBits`**: bits set from many threads at the caller's ordering, with the
  load-before-`fetch_or` the cas and image copies both use (a set bit costs a shared read and
  never takes its line exclusive).

Semantics are the strictest common ones: `contains` past the capacity is `false`, `insert`
past it panics (a fixed set) or grows (`insert_growing`), `union_with` panics rather than
drop a member past the capacity, `intersect_with` clears the words past the other set's. Every
two-set operation returns whether it changed the set, the flag a dataflow iterates on, at no
cost: it is the OR of the bits the loop computes anyway.

State of the art compared: rustc's `rustc_index::bit_set` (`DenseBitSet`, `ChunkedBitSet`,
`BitMatrix`), `fixedbitset`, `bitvec`, `hibitset`; Vigna's rank9 for `Ranked`. `Bits` matches
`DenseBitSet`/`fixedbitset` in layout and loop shape, generalizes the storage the way `bitvec`
does without its bit-order and element-type generality (which costs it whole-word loops in
general), and adds `const` inline sets. `ChunkedBitSet`'s all-zero/all-one chunks pay off only
for sets of tens of thousands of bits that are mostly uniform (rustc's move paths of huge
bodies); no caller here has that shape, so it isn't built.

## Speed of light

All on one core; numbers for an Apple M-series P-core (3 loads and 2 stores of 128 bits a
cycle from L1, about 100 GB/s from DRAM per core cluster) and Zen 4 (AVX2 or AVX-512, 2 loads
and 1 store of 256 bits a cycle).

- **`contains`, `insert`, `remove`**: one load (and a store) and two ALU ops; the bound is
  one L1 load latency (4 to 5 cycles), or throughput 2 to 3 a cycle when independent. `Bits`
  does exactly that, plus one length compare the branch predictor never misses (the
  copies that indexed unchecked paid the same compare inside `[]`).
- **`union_with`, `intersect_with`, `subtract`, `is_subset`, `count`, `is_empty`**: streaming
  over `n` words, the bound is bandwidth: `union_with` reads `16n` bytes and writes `8n`.
  From L1 that is the store port: 2 words a cycle on M-series (2 128-bit stores), 4 on Zen 4
  (one 256-bit store), so a 64-word (4096-bit) dataflow row is 16 to 32 cycles. Each is one
  zip loop with no early exit (the changed flag and `is_subset`'s test are an OR
  accumulated across the loop, not a branch per word), which LLVM vectorizes to the vector
  width; `count` vectorizes to NEON `cnt` + `addv` or AVX-512 `vpopcntq` (AVX2: the `pshufb`
  nibble count). So these sit at the store or load port bound, within 10%. From DRAM a
  union is 24 bytes a word against about 100 GB/s: 4 G words a second.
- **Iterating members** (`Ones`): per member a `tzcnt` and a `blsr` (clear lowest), about 1
  cycle; per empty word one load and one test. The bound is `members + words / k`, `k` the
  words a SIMD zero test covers; `Ones` costs `members + words`, so for a set under 1 in 64
  dense its empty-word scan is up to 2x off a SIMD skip. Every caller's sets here are a body's
  locals, blocks or paths, a few hundred to a few thousand bits, where the whole scan is tens
  of cycles; a SIMD skip pays only past about 64 KiB of words, so it isn't written.
- **`first_in`, `first`**: the same scan, stopping at the first set word.
- **`Ranked::rank`, `place`**: two independent loads (the word and its count) and a popcount.
  The loads issue together, so the latency is one load's even when both miss (memory-level
  parallelism): the bound of one cache miss is met. Rank9's interleaved layout would also
  halve the lines a cold scan touches; the callers' columns (items' rows, a sparse column's
  keys) are made once and read hot, so that isn't worth twice the code.
- **`BitMatrix::union_rows`**: as `union_with` on one row.
- **`AtomicBits::insert_if_clear`**: a relaxed load (an L1 hit while the line is shared) and,
  when clear, one `fetch_or` (`ldset`/`lock or`, 20 to 50 cycles with the line exclusive).
  The bound is that RMW once per bit ever set; repeat sets cost only the shared load.
- **Allocation**: none on any read or in-place write (asserted by
  `reads_and_writes_allocate_nothing`); `new`, `filled` and growth allocate once per set, and
  `reset` reuses the buffer, as the dataflow loops and the diagnostics passes need.
