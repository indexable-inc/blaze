---
status: plan
summary: dedup-bits-db (2026-10-29): the front end's bit sets (borrowck, hir-ty, hir-def, blaze-query, salsa) onto blaze_ids::bits, which ones stay and why.
date: 2026-10-29
---
# dedup-bits-db: the front end's bit sets

Part of [bits](bits.md). Every move is a request in `v2/requests/db.md` under
`## dedup: bits: ...`; nothing here was free to edit.

## What each copy became

| copy | becomes | why |
|---|---|---|
| `hir-ty-borrowck/src/bitset.rs` `BitSet`, `BitMatrix`, `masks`, `union_words`, `iter_words`, `words_for` | `BitSet<'f> = BitSliceMut<'f>` and `BitMatrix<'f> = BitMatrix<usize, &'f mut [u64]>` over the body's frame, with frame constructors; `Transfer` stays in `bitset.rs`, implemented for `Bits<S: WordsMut>` | mem-db-checks B2.1: a clean body makes no global allocation, so the words live in the frame |
| borrowck `constraints.rs` `from_closure` (B2: an `FVec` of words) | the `FVec` words, read and written through `BitSlice`/`BitSliceMut` views | a frame `Vec` isn't a `GrowWords` storage |
| hir-ty `infer/columns.rs` `Presence` | `struct Presence(Ranked)` | keeps its own invariant: last word nonzero, so a column has one form |
| `IdxSet<K>` | a wrapper over `BitSet` | the same invariant, and `DenseKey` keys (`Idx<T>` can't be a `BitIndex` there) |
| `VarSet`, `set_bit` | `BitSet<u32>` with `insert_growing`; deleted | no invariant of their own |
| `target_feature.rs` `TargetFeatureSet` | `struct TargetFeatureSet(BitArray<WORDS, TargetFeature>)`, `TargetFeature: BitIndex` | keeps the `Debug` that names features and the `const` API; same 72 bytes and the same hash |
| `representability.rs` `ParamsInRepr` | `Bits<SmallVec<[u64; 1]>>` | 24 bytes instead of 16, still no allocation up to 64 parameters; 8 bytes per ADT memo doesn't justify a second copy |
| `mir/held.rs` `DenseBitSet<usize>` | `BitSet` | a local analysis set |
| hir-def `item_scope.rs` `Bits` | `Bits<ThinVec<u64>>` (`ListBits`), `set_growing` | a bit per list position; the words follow the list |
| `signatures.rs` `VariantBits` | `Bits<SmallVec<[u64; 1]>>` | the same 24 bytes, asserted |
| `import_map.rs` `trait_assoc_items` | `BitSet` | one per crate |
| blaze-query `Marks` | `BitSet<MemoId>`, `insert_growing` | the same growable set |
| salsa `capture.rs` `Frame::old` | `BitSet<u32>` | the same hot path: a capacity compare, then one load, test and store |

## Kept

- `next_solver/interner.rs` `UnsizingParams(DenseBitSet<u32>)`: rustc_type_ir's
  `Interner::UnsizingParams` must deref to `DenseBitSet<u32>`.
- `layout.rs` `BitMatrix`: the argument of rustc_abi's `layout_of_coroutine`.
- `rustc_abi`: rustc's crate. Its API takes rustc_index's `BitMatrix`, and its two internal
  `DenseBitSet`s are rustc's code, generic over rustc_index's `Idx` and kept the same as upstream.
- base-db `files.rs`/`named.rs` `dirty: u64`: one word of 64 bucket flags, read with plain word
  ops. It isn't a set type, and wrapping it would only add indirection.

## Speed of light

The operations are `blaze_ids::bits`'s; see [bits](bits.md). Each move is the same work as the
copy it replaces, or less: `Presence::keys` went from 64 tests a word to one `trailing_zeros` a
member, and borrowck's joins are the shared vectorized word loops. The scratch rows in
`forward`/`backward` are still allocated once per analysis.
