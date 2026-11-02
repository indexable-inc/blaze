---
status: plan
summary: lane store's speed of light: the bound each component of blaze-cas (lookups, puts, delta tables, flags, the index table, packs and the commit, compression, chunked values, opening) and its remote tier sets, the arithmetic, today's count, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane store: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here. The sections
marked `(speed-sol2-store)` were added on 2026-10-30 by agent `speed-sol2-store`, which audited
`crates/blaze-cas` against them and closed the gaps it could. The lock-free read and write
design these sections count is in [concurrency/store.md](../concurrency/store.md), and its
costs per read and per task are in [concurrency/speed-of-light.md](../concurrency/speed-of-light.md).

## The machine

From [link's speed of light](../link/speed-of-light.md#the-machine), plus three latencies this
lane's lookups need. None of them is measured yet.

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| BLAKE3 with AVX-512 | about 6 GB/s a core |
| an L2 hit, random | about 4 ns (speed-sol2-store) |
| an L3 hit, random | about 14 ns (speed-sol2-store) |
| a DRAM miss, random | about 90 ns (speed-sol2-store) |

The reference workloads:
- **A warm build**: 10,000 units, all hits, about 100,000 gets of values of 1 to 64 KiB, against
  an index of 1 to 4 million records.
- **A cold build**: about 1 million puts, 2 GB in all, mostly 1 to 64 KiB, with a few values
  over `CHUNK_FROM` (256 KiB) and a few over `EXTERNAL_FROM` (4 MiB).
- **A body edit**: tens of puts and hundreds of gets.
- **Opening the store**: a mapped index of 1 to 4 million records and a delta of up to
  `INDEX_AFTER` (65,536) records.

## `blaze-store` and the v1 remote

`crates/blaze-store` is replaced by `blaze-cas` and deleted at integration, and the v1 remote
sync in `crates/blaze-cargo/src/remote.rs` and `remote/` goes with it
([crates.md](../../reference/crates.md)). Neither gets a bound; the work goes into `blaze-cas`.

## Lookups (speed-sol2-store)

`Look::find` in `read.rs`: per tier, the delta table and the one before it (`table.rs`), then
the mapped index table (`index.rs`); then `Look::value` reads the record, hashes it on its
first read and sets the `Read` flag.

**The work, for a hit on the index**: the key's control group (16 bytes of the control column,
1 byte a slot), its row (64 bytes: the key and its place), the record's header and payload, and
the record's flags. The control column of a 4-million-record index is 4.6 MB and the flags are
4 MB, so both stay in L3 on a warm daemon; the row and the record are DRAM misses.

**The bound**: two DRAM misses plus one L3 hit, 90 + 90 + 14 = 194 ns. The first read of a
record also hashes it: its length / 6 GB/s, once per daemon (the `Verified` flag lives in
memory only).

**Today**, counted from the code: a probe of the delta (one slot line, L2, 4 ns, plus 0.4 to 1.5
dereferences of a non-empty slot into the values region as the delta fills from a quarter to a
half, 6 to 21 ns), the control group (14 ns), the row (90 ns), the record (90 ns), and the flags.
Before this round the flags were one bit column a flag, so `Verified` and `Read` were two lines
(28 ns); now a row's six flags share one 64-byte line (14 ns, the "Flags" section). That makes a
hit 212 to 233 ns against 194: 9% to 20% over, 14% on average across a delta's cycle.

**The gap** is the delta probe's dereferences on a miss in the delta, which every index hit
pays. It closes by putting a 32-bit fingerprint of the key beside the pointer in each delta
slot (an `AtomicU64` of fingerprint and value index, instead of an `AtomicPtr`), so a probe
that misses dereferences only on a fingerprint match (one in 2^32). That would bring a hit to
198 to 204 ns, within 5%. It is not done: it changes the delta table's unsafe core and its model
tests, and needs the model checker run against it (queue 00912, lane store).

**A hit on the delta** (a record put this period): the slot (4 ns), the spot (14 ns), the
record in its segment chunk or pack (90 ns). The spot holds the place, so it is part of the
work: 108 ns against a bound of 108.

**A hit on a value made whole** (zstd, LZ4, chunked): the lookup above, then the `Owned` table
(one probe, about 18 ns); nothing is decompressed again.

**What `proof` should measure**: per get hit, cache lines touched (a counter: 4 on the index
path, 3 on the delta path), 0 allocations (`blaze-alloc-count`, already in `tests.rs`), and
the time of 100,000 random hits against a 4-million-record index (budget: 210 ns each).

## Puts (speed-sol2-store)

`Writer::put` in `segment.rs` and `lib.rs`: hash the value (BLAKE3 for the content id), check
the key is not kept (`Look::kept`), encode the record into the worker's own segment chunk,
publish its spot in the delta with one compare-and-swap.

**The work, per put of n bytes**: one hash pass (n / 6 GB/s), one copy into the segment (n /
25 GB/s from DRAM, faster from cache), one 96-byte header, one compare-and-swap.

**The bound**: n × (0.167 + 0.04) ns + a probe and a compare-and-swap (about 25 ns). For a 4 KiB
value: 0.68 + 0.16 + 0.03 = 0.87 µs. A cold build's 2 GB on 16 workers: 2 GB × 0.207 ns / 16 =
26 ms of CPU, under the device's 0.29 s.

**Today**: exactly that, plus a second delta probe (`kept` walks every tier, then the insert
walks the delta again): about 10 ns, 1% of a 4 KiB put. Within 10%.

**What `proof` should measure**: hash passes per put byte (1), user copies before the commit
(1), allocations per put (0 below `CHUNK_FROM`).

## Delta tables and their words (speed-sol2-store)

`Table<T>` in `table.rs`, over `Words` in `words.rs`: levels that double, each a reserved
region committed whole when made, now on huge pages (`Region::reserve_huge`, lane mem).

**The work**: a delta of 65,536 records is a first level of 131,072 slots (`DELTA_FIRST`), 8
bytes a slot (1 MB) plus the spots (about 130 bytes each, 8.5 MB), plus its flag words.

**The bound and today**: the probe costs are in "Lookups"; making a level is one reservation
and one commit of its bytes, once per index swap. Huge pages make a probe one TLB entry per 2
MiB instead of one per 4 KiB. Within 10%.

## Flags (speed-sol2-store)

`FlagColumns` in `flags.rs`: six flags a record (`Verified`, `Read`, `ReadBefore`, `Tried`,
`New`, `NewBefore`) over an index table's slots.

**The work**: a hit loads `Verified` and sets `Read`. A settle rotates (`Read` into
`ReadBefore`, `New` into `NewBefore`) and sweeps flags 64 records a word.

**The bound**: one cache line a hit, already in L3 (14 ns), and a load before any write, so a
flag already set writes no shared line.

**Before this round**: one bit column a flag, so the two flags of a hit were two lines a
column's length apart (28 ns, 2 lines).

**Changed (speed-sol2-store)**: each 64 rows have one 64-byte line of eight words, a word a
flag (two words padding), so every flag of a record is on one line: 1 line, 14 ns, at the
bound. It costs 1 byte a row instead of 0.75 (4 MB for 4 million rows). A rotation now touches
every line (rows bytes, 4 MB in 0.2 ms) instead of four columns' lines (2 MB), once per settle
period. Tests in `flags.rs`: a row's flags share one line, and the blocked layout answers as a
`Vec<bool>` per flag does over 100,000 random sets, rotations and rows past the end.

**What `proof` should measure**: lines touched per hit for the flags (1).

## The index table and its rebuild (speed-sol2-store)

`build_index` in `index.rs`, called from the commit (`Store::start_commit` in `lib.rs`) once the
delta holds `INDEX_AFTER` records: every record of the old index and the delta goes into a new
table, written whole.

**The work, per rebuild of N entries**: read N entries (72 bytes each), write the table (64 +
65 bytes a slot, with between 8/7 and 16/7 slots an entry, about 1.5 on average, so about 98
bytes an entry), and write the file.

**The bound**: memory, 170 N bytes / 25 GB/s; for N = 4 million, 680 MB in 27 ms. The file
(390 MB) is written by the driver at 7 GB/s in 56 ms, off the store thread.

**Before this round**: `start_commit` sorted the entries by key (a comparison sort of 72-byte
entries: 4 million × 22 comparisons, 300 to 450 ms), `build_index` copied them (`to_vec`, 288
MB) and sorted them again, then placed them in key order. Key order is uncorrelated with the
group a key starts at, so each placement was a random miss in the control column and a random
64-byte row write: 4 million × 100 ns, 150 to 400 ms with the misses that overlap. About 0.6 to
0.9 s a rebuild against 27 ms: over 20x.

**Changed (speed-sol2-store)**: the commit no longer sorts. `build_index` sorts only `(placing
value, entry number)` pairs (8 bytes each, 4 radix passes with `blaze_ids::radix::sort_pairs`),
where the placing value is the key's start group in the high bits and key bytes 8 to 12 below.
Equal values (rare: one group in about 200 has a pair) are sorted by key. Entries are then
placed in that order, read through their numbers. Each probe starts at or after the group the
one before did, so the control column and the rows are written front to back, and the old
index's entries (most of N, listed in slot order) are read nearly in order. The bytes still
depend only on the set of entries: the order is (placing value, key), a total order on keys.
Counted: placing values 288 MB read (12 ms), the sort 256 MB (10 ms), placement 288 MB read and
390 MB written in order (27 ms), and 95,000 first touches of the output's pages (24 ms): about
75 ms, 10x faster than before.

Tests in `index.rs`: placing by group answers every lookup (present, absent and same-group
neighbours) as placing in key order did, over seven sizes and a clustered set; the placing
order never goes back a group and lists each entry once; keys tied on the placing value still
give bytes that depend only on the set. The existing tests (every key found, load at most 7/8,
the same bytes for any order of the entries, a repeated key refused) still hold.

**The gap that remains**, 75 ms against 27 ms of memory, or 56 ms of device time:
- The output's page faults (24 ms): a reused output buffer, or the table built on huge pages,
  removes most of them.
- `index_entries` copies every old index row into a `Vec` before the build (288 MB). Merging the
  mapped old table (already in slot order) with the sorted delta directly would remove that copy
  and the gather.
- **The schedule**: the whole index is rewritten every 65,536 records, so a cold build of M
  records writes about M² / 131,072 entries of index in all: 7.6 times one final write at a
  million records, 30 times at four million. A threshold that grows with the index (rewrite when
  the delta reaches half the index) bounds that to 3 times, but records not in the index are
  found at open by scanning their pack headers, so a bigger delta makes every open slower. Both
  go away with a two-level index: a base table rewritten geometrically and a small run table
  written per 65,536 records, a lookup probing both behind one fingerprint check. That is a
  change of the store's format: queue 00912, lane store.

**What `proof` should measure**: the time of a rebuild of 4 million entries (budget: 80 ms
now, 30 ms after the merge), and the index bytes written per record put over a cold build of a
million records (7.6 entries' worth now; budget 3 after the schedule change).

## Packs and the commit (speed-sol2-store)

`Store::start_commit` in `lib.rs`: drain every segment into pack memory, then per pack
`Truncate`, `WriteAt`, `Sync`; then external files, the index and the head.

**The work, per byte put**: write it to the device once (1 / 7 GB/s = 0.14 ns).

**The bound**: 0.14 ns a byte, with the store thread's own work under 10% of it, so at most one
copy overlapped with the device.

**Today**: two user copies on the store thread before the device: the drain from the segment
into pack memory (0.1 ns a byte), and `pack.memory[..].to_vec()` for the `WriteAt` (0.1 ns a
byte, plus the page faults of a fresh `Vec`: 0.06 ns a byte), kept because a failed write must be
retried from the same bytes. Commits are serial, so a commit runs at about 0.4 ns a byte
against 0.14: about 3x off. External files are cloned into their `Write` the same way.

**What stands in the way**: `Effect::WriteAt` owns its bytes and `Done::Written` gives nothing
back, so the store cannot lend pack memory to the write and take it back on failure. Queue
00910 (lane core) asks for the buffer back on completion and failure; then the commit moves
the drained tail into the effect, leaving one copy (0.1 ns, overlapped with the device's 0.14
by pipelining the drain of the next pack). The drain itself goes only if segments are written
in place (a gather write from segment chunks), which needs headers and padding laid out in the
chunk exactly as in the pack.

**What `proof` should measure**: user copies per committed byte (2 now, 1 after 00910), and
commit throughput on the sim driver against a 7 GB/s device model.

## Compression of cold values (speed-sol2-store)

`cold.rs`: zstd at level 6 with a dictionary per kind for cold values, long-distance zstd for
values over `LONG_FROM`, LZ4 for chunks.

**The work, per value**: compressing or decompressing its bytes. A small value (1 to 16 KiB)
decompresses in 2 to 8 µs and compresses in 10 to 40 µs at level 6.

**The bound**: zstd's own speed on the bytes, with no setup per value.

**Before this round**: every put made a fresh compression context and every read a fresh
decompression context (`zstd::bulk::Compressor` and `Decompressor`, made and dropped per
call). A decompression context is about 160 KB, so making one is an allocation over the mmap
threshold, its page faults and its unmapping (about 10 µs), more than decompressing a 4 KiB
value. A compression context allocates its level-6 tables at its first frame, likewise.

**Changed (speed-sol2-store)**: each thread keeps one compression and one decompression context,
made at its first use. A compression references the kind's digested dictionary, compresses
with `compress2`, then resets session and parameters, so the next value starts from a context
as new and the context never keeps a pointer to a dictionary. The bytes are the same as a
fresh context makes. Decompression passes the dictionary per call (`decompress_using_ddict`),
so the context keeps none. Tests in `cold.rs`: values of two dictionaries and long frames
interleaved on one thread compress to the bytes a fresh context makes and read back, the wrong
dictionary is refused, and four threads make the same bytes. At the bound.

**What `proof` should measure**: the time of 10,000 gets of 4 KiB cold values (budget: zstd's
decompression of those bytes plus 10%).

## Chunked values (speed-sol2-store)

`chunk.rs` and the put of a value over `CHUNK_FROM`: content-defined chunks (gear hash), each
chunk LZ4-compressed and put under its own key, then a list record.

**The work, per byte**: the gear pass (about 0.1 ns with AVX-512), LZ4 (about 1.4 ns a byte on
one core), and hashing.

**Today**: three BLAKE3 passes per byte (the chunk's key, the chunk's content id, the value's
content id), 0.5 ns a byte. For a 100 MB value: LZ4 140 ms, hashing 50 ms, the gear pass 10
ms: 200 ms. One hash pass fewer (the chunk's key derived from its content id) saves 17 ms, 8%:
within 10%, so it is left; it would change every chunk key, so new and old chunks would not
dedupe against each other.

**What `proof` should measure**: hash passes per chunked byte (3), and the time of a 100 MB put.

## Opening a tier (speed-sol2-store)

`Store::open` and `open_tier` in `lib.rs`: map the index, check its CRC32C and its control
column, scan the delta records of packs past their indexed length, map the packs.

**The work**: the index bytes once (CRC32C at about 20 GB/s; 260 MB in 13 ms for 4 million
records), the control column once (8 bytes a step), one header per delta record (65,536 at
most, mostly a page each: 16 ms), one `mmap` per pack.

**Today**: that. Within 10% of reading those bytes once. The schedule change in "The index
table and its rebuild" would make the delta scan the larger part.

**What `proof` should measure**: the time of an open of a 4-million-record store with a full
delta (budget: 35 ms).

## The remote tier

The v1 remote sync in `crates/blaze-cargo/src/remote.rs` and `remote/` is deleted at integration
(`v2/requests/store.md` request 2), so it gets no bound. The v2 tier is `crates/blaze-cas/src/remote.rs`,
which belongs to agent `concurrency`. Its bound belongs to that agent, and is written here or in
`blaze-cas`'s notes. `speed-link-edge-remote`'s measurement of that tier, and the requests C1
to C8 (`v2/requests/concurrency.md`) and K1 and K2 (`v2/requests/core.md`), give the inputs.

The bound a fetch has, per byte and per hit:
- **A hit**: one round trip for a batch of lookups. With `FindMissingBlobs`-style batching, a
  build of 10,000 units asks once, not 10,000 times. Over 1 Gbit/s with 20 ms of RTT, the bound
  is one RTT (20 ms) plus 10,000 × 32 bytes of keys / 125 MB/s (2.6 ms).
- **Bytes**: the network's bandwidth, and one copy into the pack: 1 GiB over 10 Gbit/s is 0.86 s.
  The local copy (about 10 GB/s) and the hash (BLAKE3 at about 6 GB/s a core, on 4 cores 24 GB/s)
  are each under 5% of that.
- **What stands in the way** (from the measurement): today 5 copies and 2 hash passes per byte,
  one send per answer, and head-of-line blocking inside a batch. C2 to C7 and K2 bring that to
  2 copies and 1 hash pass. At 10 Gbit/s that is within 10% of the network bound, and at 100
  Gbit/s the copies are the bound: then K1 (zero-copy send) and K2 (receive into place) are
  what reach it.
- **Corrected (speed-sol2-store, from the code on 2026-10-30)**: C1 to C8 are done, and a
  fetched value now takes one hash pass. The "2 copies" left out the store's side: a value
  received is copied into a segment (`put_hashed`), then drained into pack memory and copied
  again into the `WriteAt` at the commit ("Packs and the commit"), so 3 user copies (4 for a
  frame small enough to be copied out of the receive buffer). The copies run on the store
  thread while later bytes arrive, so at 10 Gbit/s (0.8 ns a byte against 0.3 of copies) the
  tier is still at the network bound; at 100 Gbit/s queue 00910 and K2 are what reach it.
