---
status: plan
summary: dedup-bits-edge (2026-10-29): the bit sets of blaze-usercode, blaze-units, blaze-cargo, blaze-link and blaze-inproc moved onto blaze_ids::bits, what stays and why, and the speed of each.
date: 2026-10-29
---
# dedup-bits-edge: the edge crates' bit sets

Part of [dedup-bits](bits.md). Every copy below is moved by a request to its owner; none of
these crates was free.

| copy | becomes | why |
|---|---|---|
| `blaze-usercode` `test/queue.rs` `Bits` | `Ranks`, a `BitSet<u32>` and a `low` cursor | the cursor is the queue's own invariant: `first` scans only forward until a lower bit is set |
| `blaze-units` `stdlib/target.rs` `Bits` | `Features = BitArray<4, u16>` | same 32 inline bytes, `Copy` |
| `blaze-cargo` `serve/unit/graph.rs` `Bits` | `BitSet<u32>` | same operations |
| `blaze-cargo` `serve/job.rs` `reported` | `BitSet<u32>` | an inline test-and-set |
| `blaze-link` `native.rs` `Bits` | kept, renamed `CowFlags`, using `locate` | its words are a `CowVec`'s shared chunks, copied on write, not one slice |
| `blaze-link` `cow.rs` `IdSet` | `bits` and `listed` become `BitSet<u32>` | the first-added list beside them is the wrapper's own |
| `blaze-link` `commit.rs` `done` | `BitSet`, `reset` per step | buffer kept across steps |
| `blaze-link` `macho/dylib.rs` `seen` | `BitSet` | one test-and-set per trie node |
| `blaze-link` `elf/read/strings.rs` `nuls` | `BitSet` with `first_in`, `last_in`, `rank_in` | not `Ranked`: the counts are made in the pass that finds the NULs, which `Ranked` would redo |
| `blaze-inproc` `fence.rs` `used` | `BitArray<4>` with `first_absent`, `last`; `high` kept | same 32 bytes |

Not sets, left alone: the GNU-hash bloom words in `blaze-link` (`elf/dynamic.rs`), the timing
wheels' per-level occupancy words (`blaze-usercode` `test/queue/wheel.rs`, `blaze-core`
`wheel.rs`), the signal mask in `blaze-live` `linux/stop.rs` (a kernel `sigset_t`), and
`blaze-graph` `pump.rs` `KeyBits` (a `Hasher`).

## Speed of light

Each of these is a word operation per member touched, already at the bound before the move:
a test-and-set is one load and one store of a word in L1 (about 1 ns), a scan is one
`trailing_zeros` (or `leading_zeros`) per word, 64 bits per cycle. The move keeps every one of
them the same instruction sequence: `insert`, `contains` and `remove` are the same `locate`
and mask; `first_in` and `last_in` scan word by word as the string table did; `rank_in` is the
same load of the count and one popcount; `first_absent` is the same `trailing_ones` of the
first word not all ones. Nothing new allocates: `reset` keeps the commit's buffer, the
queue's and the units' sets are made once, the feature sets stay inline. The target's
feature union, difference and subset are four words each, vectorized as before.
