---
status: plan
summary: blaze v2 proof lane, hot_lint pass on blaze-v2-proof: allocation per first difference, join and trace file, before and after, and the budget tests that hold them.
date: 2026-10-29
---
# speed-link-edge-hot-proof: allocation on the oracle's hot paths

Part of [proof](../proof.md), after [speed-link-edge-proof](speed-link-edge-proof.md) and the
bounds in [speed-of-light.md](speed-of-light.md). Paths: `crates/blaze-v2-proof/`. Nothing here was
built or run (docs/v2/plan/goals/principles.md); counts are read off the code.

## The hot paths

A proof run compares two traces of a build (millions of keys, tens of millions of edges for
nushell) and, on a mismatch, two outputs. So the hot paths are the first difference
(`bytes.rs`), reading a trace in place (`wire.rs`, `View::parse`), and the joins `under` and
`over` (`join.rs`, generic over `columns`). The steady state is two traces that agree. The
oracle's answer is then empty, and the work itself allocates nothing. `main.rs` and `bench.rs`
are the CLI edge and run once a process.

## State of the art

- First difference: glibc's `memcmp`, which allocates nothing.
- Reading a file in place: a zero-copy format (FlatBuffers, Cap'n Proto, rkyv), which allocates
  nothing past the validation.
- A merge join of two sorted columns, as in a column store: the output buffer and nothing else.

## Before and after

| Path | Before | After |
|---|---|---|
| First difference across cores | a `Vec` of one answer a piece, plus `available_parallelism` each call (on Linux, cgroup files read: syscalls and allocations) | one atomic minimum over offsets; the core count asked once (`join::cores`) |
| `join::split` | a `Vec<T>` of piece answers, then flattened and collected again | each answer handed to a `take` closure in range order; one piece runs on this thread with no allocation |
| `under` / `over` | a piece `Vec` per piece, then a collect into the answer, then a stable sort (buffer) | the first piece's `Vec` becomes the answer and later ones append; an unstable sort, valid because a query is stale at most once; nothing allocated while the answer is empty |
| `View::parse` checks | two `split` results collected | folded into one `Option` each |
| Writing a trace (`Serialize`) | eight column `Vec`s, then the encoder put the map's fields in order through a copy the size of the trace | one column buffer, sized once for the longest, filled and written per column in the keys' encoding order |
| `Trace` bitsets | a local `Bits` | `blaze_ids::bits::BitSet<u32>` (lane dedup), the same words |

## Budget tests (`blaze-alloc-count`, counting allocator at the crate root)

- `bytes::tests::a_first_difference_allocates_nothing`: every SIMD path, a short input, the
  fastest path on equal bytes and one piece on this thread: 0.
- `join::tests::a_join_of_traces_that_agree_allocates_nothing`: `under` and `over` on one piece,
  owned and in place, warm (twice): 0.
- `wire.rs` tests: a trace written, 1 (its one column buffer); parsed and checked in place on one
  piece, 0; copied out, 8 (one a column).

## `lib.rs`

`Trace::edges`, called per key in every join, and `TraceBuilder::finish` index by two `usize`s
rather than cloning a `Range`. `finish`'s buffers are each sized once a trace. `sample` sorts
its truncated index list in place instead of copying it out first. The answers of `under`,
`over` and `apart` stay unallocated while empty. `Trace` keeps `Clone` for the tests only;
`Stale` owns no heap. The hot lint finds nothing in the crate.
