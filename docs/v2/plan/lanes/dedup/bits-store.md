---
status: plan
summary: dedup-bits-store (2026-10-29): the bit sets of blaze-items, blaze-cas, blaze-image, blaze-v2-proof, blaze-vfs, span, syntax-tree, parser, intern and hir-ty-ub moved onto blaze_ids::bits, what stays and why, and the speed of each.
date: 2026-10-29
---
# dedup-bits-store: the store's and the format's bit sets

Part of [dedup-bits](bits.md). None of these crates was free, so every move below is a
request to its owner, under `## dedup: bits: <crate> ...` in `v2/requests/items.md`,
`store.md`, `frontend-image.md`, `proof.md`, `vfs.md`, `format.md` (span, syntax-tree, parser,
intern) and `safety.md` (hir-ty-ub).

| copy | becomes | why |
|---|---|---|
| `blaze-items` `table.rs` `Bits` | `Ranked` (`from_bools`, `place`) | the same words and per-word ranks, one column allocation (`from_bools` reserves by the iterator's size) |
| `blaze-items` `form.rs` impl items used (`u64` plus an `FxHashSet` past 64) | `Bits<SmallVec<[u64; 1]>>` | the same inline word for up to 64 items, words not a hash set past that |
| `blaze-items` `form.rs` `CanonBuffers::uses` | kept, `Vec<Cell<u64>>`, using `words_for` and `locate` | marked through `&self` during the walk, so the words are cells |
| `blaze-cas` `flags.rs` `Bits` | `FlagColumns`, each column an `AtomicBitSlice` (`insert_if_clear`, `move_into`) | the words are a store region's, not a box an `AtomicBits` could own |
| `blaze-cas` `table.rs` flag bits | the same `AtomicBitSlice` per level and column | one bit layout for the table and its flags |
| `blaze-image` `checked` / `bad` | `AtomicBits` | same words, same `Relaxed` orderings |
| `blaze-v2-proof` `Bits(pub Vec<u64>)` | `BitSet<u32>` | same words; the wire writes `words()` and reads `from_words`, so the bytes are the same |
| `blaze-v2-proof` `wire.rs` `bit` | kept, using `locate` | reads little-endian words in place from a mapped file, unaligned |
| `blaze-vfs` `journal.rs` `Written` | `BitSet<NodeId>` (`NodeId: BitIndex`), the renumbering a free function | same operations; `grow` never shrinks, and nothing read the shrunk tail |
| `blaze-vfs` `tree.rs` `collect`'s `reached` | `BitSet<NodeId>` | a test-and-set and an ordered walk |
| `span` `ast_id.rs` slot words | `BitArray<{SLOTS / 64}, u32>` with `insert_range` | same inline words |
| `span` `lines.rs` `LineIndex` | kept, using `below` | a rank per 16-byte block in a `u32` per 64 words: 1/16 bit per byte, where `Ranked`'s rank per word would be 8 times the table |
| `syntax-tree` `flat.rs` `Kept` and the node ranks | `Ranked`, `ranks_of`, `rank_in`; the splice is `BitSet` with `or_range_at` | same words and ranks, the splice a word at a time as before |
| `parser` `token_set.rs` `TokenSet` | `BitArray<3>` const `with_bit`, `or`, `has_bit` | same 24 const bytes |
| `parser` `lexed_str/scan.rs` class blocks | kept | interleaved per-class words written by SIMD a block at a time, not one set |
| `intern` `table.rs` `Base::live` and `Live` | `BitSlice<'static>` over the image's words; `BitSet<u32>` | same words, same written bytes |
| `hir-ty-ub` `borrows.rs` GC live set | `BitSet`, `reset` per GC | buffer kept between runs |
| `hir-ty-ub` `memory.rs` leak check | `BitSet` | one test-and-set per allocation |
| `hir-ty-ub` `memory/bytes.rs` init bits | kept, `words_for` and `locate` | empty means every byte initialized, and the words move by `init_mask`'s blits |

Not sets, left alone: `blaze-cas` `index.rs`'s control-word counts, `blaze-format`'s LEB128
stop bits, `tt`'s varint shifts, `blaze-store` `remote_index.rs`'s entries.

## Speed of light

Each copy was already a word operation per member touched, and stays the same instruction
sequence: `contains`, `insert` and `remove` are one `locate` (a shift and a mask) and one load
or store in L1, about 1 ns; the atomic ones are one `fetch_or` or `fetch_and`, `Relaxed` as
before. `Ranked::place` is the stored count of the word and one masked popcount, as the
items table and the syntax tree did. Scans (`iter`, `first_in`, `count`) go a word at a time,
64 bits per `trailing_zeros` or `count_ones`. `or_range_at` moves a range of bits as the
syntax tree's `append_bits` did, two shifts per destination word. Nothing new allocates: the
GC and the interner reuse their buffers, `from_bools` allocates its columns once, the token
sets and the slot words stay inline and const, and the image and store columns keep their
words where they were mapped.
