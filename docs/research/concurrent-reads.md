---
status: record
summary: The store's cache of unpacked blocks on a warm start, and ordered and hash maps for blaze's shared maps.
date: 2026-10-24
---
# Concurrent reads: the store's block cache, ordered maps and hash maps

2026-10-24. What the compiling threads share and read together: whether the store's cache of
unpacked blocks is what a warm start waits on (measured), and what bbtree, a concurrent B+tree
posted that week, and papaya, a concurrent hash map, would change for blaze's maps. Each claim
is **read** (measured, or read in the source), **reported** (someone says so) or **unverified**
(our inference). What was fetched is in [docs/upstream/concurrent-maps/](../upstream/concurrent-maps/).

## Short version

- The cache of unpacked blocks is not what helix's warm start waits on. A block unpacked once
  however many threads ask for it, and the cache from 32 MiB to 1 GiB, take helix's "again" from
  37.0 s to 36.6 s at best, for 1.1 GB more
  daemon memory. The difference is within the 1.1 s between base and the change at the same
  size. Not shipped (**read**).
- The store's values are read about one to a block, not in the order they were added as the
  store's comment on `BLOCK` expects: at 32 MiB, 238k values cost 151k block unpacks of about
  220 KB each, 33 GB inflated. Even at 1 GiB, unpacking is 19 s of the 107 s of
  "store read" thread time, and "store read" is spread over 64 threads (**read**). The rest of
  "store read" is not unpacking: it is the decoding and interning of what was read
  ([warm-start](https://github.com/indexable-inc/blaze/blob/warm-manifest/docs/research/warm-start.md),
  not on main yet, says each set of types is interned type by type) (**unverified** how much).
- bbtree is ordered; papaya, DashMap and the interner's shards are hash maps. For point lookups
  hashbrown does a u64 get in 17 ns where bbtree takes 130 ns one key a call and 75 ns batched,
  10M entries, on bbtree's own single-thread numbers (**reported**). blaze's shared maps (the
  interner, salsa's tables, `Entries`) are looked up by key, not scanned in order
  (**unverified** beyond `cached.rs` and the interner), so bbtree has no place in blaze today.

## The store's block cache, measured

A sealed pack keeps its values in zstd blocks of at least `BLOCK` (128 KiB) of values; a value read
from one unpacks its whole block into a cache of `UNPACKED` (32 MiB) shared by every thread,
under one lock, the least recently used block evicted first (**read**, `blaze-store`). The
change tried: the first thread to ask for a block unpacks it and the others wait for that copy
(a `OnceLock` per block, filled outside the lock), eviction in batches down to 7/8 of the
size, the size from `BLAZE_CACHE_UNPACKED`, and a line in `BLAZE_STATS` counting blocks
unpacked and values read from blocks already unpacked. A test had 16 threads read every value
together and asserted one unpack per block; with the per-block cell removed it failed three
runs of three (41,936 unpacks for 6 blocks) (**read**).

Measured on dev-aws-7 (c7i.16xlarge, 64 threads, Linux 6.18), `harness/warmstart.py helix` from
`warm-manifest` at 6221f92 (the entry single-flight and interner fixes, not yet on main) against
the same with the change, Cranelift, rustup's 1.99.0, one run each. "store read" and the block
counts are from helix's "again" worker, its first build (**read**):

| | base (32 MiB) | change, 32 MiB | change, 256 MiB | change, 1 GiB |
|---|---:|---:|---:|---:|
| "again", build s | 37.03 | 38.09 | 36.95 | 36.60 |
| "store read", thread s | 121.7 | 137.8 | 121.4 | 107.2 |
| blocks unpacked | not counted | 151,088 | 111,901 | 57,889 |
| MB unpacked | not counted | 33,496 | 24,420 | 12,713 |
| unpacking, thread s | not counted | 43.8 | 35.1 | 19.2 |
| values from blocks unpacked before | not counted | 86,600 | 126,109 | 180,064 |
| daemon resident, MB | 6,229 | 6,457 | 6,536 | 7,295 |
| peak, MB | 11,973 | 12,783 | 12,409 | 13,163 |

The second build in the same worker, which compiles what the store had for the first edit to
patch, read 12.8 s of "store read" on base and 7.5 s at 1 GiB (11,784 blocks unpacked against
45,826 at 32 MiB) (**read**).

What would make the reads cheaper, not tried: smaller blocks, or values the warm start reads
together kept in the same block. Unpacking is under a second of the build's wall time on 64
threads (**unverified**: 19-44 s of thread time spread evenly), so neither is worth it before
the rest of "store read" is.

## bbtree, papaya and blaze's maps

bbtree ([XiangpengHao/bbtree](https://github.com/XiangpengHao/bbtree) at 8adae97, 0.1.5, MIT OR
Apache-2.0) (**read** unless marked):

- An ordered map. Inner nodes are read under optimistic version locks (a reader writes nothing
  shared, and retries if a writer moved the version); leaves are latched. A freed node goes on
  a free list and is not returned to the allocator while the map lives, so a late reader always
  reads a node; the map's memory does not shrink.
- No `&V` from `&self`: values are copied out (`get_copied`) or read in a closure; a scan
  walks one leaf at a time and is not a snapshot.
- A batch API (`get_many_copied`, `insert_many`) runs each key as a small state machine that
  prefetches the next node and yields to the next key while the line arrives. On 10M entries,
  one thread, its lookups are 1.45-2.53x faster than one key a call; a plain loop over all keys
  level by level was a further 10-30 ns a lookup faster and was replaced for readability. One run per cell,
  runs that fetched more lines than usual discarded and run again.
- Its own concurrent round: at 24 threads, u64 gets, bbtree 111.7 M/s and DashMap 209 M/s; the
  blog reports bbtree ahead of DashMap at 96 threads (**reported**). papaya was not measured.
- The end-to-end claims (LanceDB 4x, SlateDB 2.4x, Turso 2.2x, fjall 3x) are one thread, each
  project's own benchmark of its memtable, on a Ryzen 9 9900X (**reported**).
- Young: first commit 2026-09-19, about 33k lines written by an LLM, many `unsafe` blocks,
  checked with Verus, Shuttle, fuzzing and Miri (**reported** for the checks).

papaya ([ibraheemdev/papaya](https://github.com/ibraheemdev/papaya) 0.2.5, MIT) is a lock-free
hash map for read-heavy work: a read pins a `seize` guard and gets `&V`, memory is reclaimed
once no guard can see it, and the table resizes incrementally (**read**, README). Unordered,
so no ranges.

For blaze (**unverified**, from the above): a map read by key from every core wants a hash
map whose reads write nothing shared (papaya, or shards under read locks, as the interner now
does on `warm-manifest`); bbtree only where order or ranges are needed, which no shared map in
blaze is today. Batching lookups to overlap their cache misses would apply to a hash map too,
more simply (every bucket's address is known from its hash up front), but no measured cost in
blaze is a map's cache misses.
