---
status: plan
summary: lane concurrency's speed of light: what a read, a put and a task cost in shared cache lines, atomics and fences in blaze-cas and blaze-reclaim, against the bound of no shared write on a read, and the arithmetic.
date: 2026-10-30
---
# Lane concurrency: speed of light

Written by agent `speed-sol2-store` on 2026-10-30, from the code. The bar is "Within 10% of the
theoretical maximum, everywhere" in [principles.md](../../goals/principles.md); the design it
counts is [store.md](store.md), the rule [design/concurrency.md](../../design/concurrency.md).
The machine figures are those of [store's speed of light](../store/speed-of-light.md#the-machine).
What a lookup, a put and a commit cost in memory and device time is counted there; this file
counts only what the concurrency design adds to each: shared lines written, atomic
read-modify-writes and fences.

## Reads (speed-sol2-store)

`Reader::get`, `get_in_place` and `has` in `crates/blaze-cas/src/read.rs`.

**The bound**: a read shares nothing but published, immutable data, so it adds no write to a
shared line, no read-modify-write and no fence: only acquire loads of lines that do not change.

**Today**: one acquire load of the view pointer (a line only the store writes, once per
publish), acquire loads of delta slots, and the flags. A flag already set is only loaded; one
not set takes one relaxed `fetch_or`, once per record and period (`Read` once per settle period,
`Verified` once per daemon). So the steady state writes nothing shared, and a period's first
read of a record writes one flag line. At the bound.

**What `proof` should measure**: shared-line writes per repeated hit (0), read-modify-writes
per first read of a period (at most 2: `Verified`, `Read`).

## Puts (speed-sol2-store)

`Writer::put` in `crates/blaze-cas/src/segment.rs` and `lib.rs`.

**The bound**: one read-modify-write to publish the record where every reader can find it, and
no line another worker writes.

**Today**: each worker writes its own segment chunk; publishing is one compare-and-swap on the
key's delta slot. An insert that finds a level half full seals one slot (one more
compare-and-swap, once per level and run). At the bound.

**What `proof` should measure**: compare-and-swaps per put (1, plus the seals), and lines two
workers both write between commits (0).

## Reclamation (speed-sol2-store)

`crates/blaze-reclaim/src/lib.rs`: `enter` and `Guard`, `Grace`, `Limbo`.

**The work, per task**: the outermost `enter` loads the epoch, stores it in the thread's own
128-byte slot with release and issues one `SeqCst` fence; its guard's drop is one release
store. About 10 to 30 ns a task (the fence dominates), and nothing per read.

**The work, per retirement**: `Grace::start` is a fence and one `fetch_add`; `passed` reads
each thread's slot, one line a thread (64 threads: 64 L3 or remote hits, about 1 to 6 µs), and
only the store's owner calls it, at its own pace.

**The bound**: per task, one store to a line the thread alone writes plus the fence the
store-load order needs (`enter` against `Grace::start`: without it a writer's scan could miss a
reader that just entered). A task is microseconds or more, so 30 ns is under 1% of the
shortest. At the bound.

**What `proof` should measure**: fences and shared-line writes per task (1 and 0), and the time
of `passed` with 64 threads (budget: 10 µs).

## The remote tier

`crates/blaze-cas/src/remote.rs`: its copies and hash passes are counted in the store's file
("The remote tier", corrected on 2026-10-30).
