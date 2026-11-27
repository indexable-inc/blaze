---
status: plan
summary: blaze's plain store (crates/blaze-cas/src/plain) against Relax's store, row by row, for a repeat after a restart that reads few pages, and for records fetched from a static origin by byte range (an option, off unless chosen). Where each adopted piece lives (the sealed exit tail and its background pass, the lookup, the startup image, per-worker puts, the machine scan, batches, the per-segment Bloom filter, the D182 block bottom); what was left and why; what is still to build (the scrub in the helper, blaze-run reading the startup image, held descriptors, two writers on one device); the digest decision.
date: 2026-11-15
---
# The store, next

The store is `crates/blaze-cas/src/plain` (its module doc is the format). The reference is Relax's
`compiler/src/store.rs` (MIT, read, not run). Goal order: incremental compile (after a restart
too) and lints, then memory, cold compile, disk cleanup, disk. Remote records are supported and not
in the order; fetching them is an option, off unless chosen.

## Side by side

| row | blaze (plain store) | Relax (`store.rs`) | better, and what changes |
|---|---|---|---|
| file layout | `head` (two 64-byte slots a sector apart), `manifests-<id>` logs, `seg-<id>` files written once, one namespace each, 16-aligned payloads, a sorted index of 48-byte entries and a BLAKE3 footer | one file per crate version: a 96-byte header with two slots, two index areas sized for a capacity, payloads appended past both slots' data ends | blaze: a segment is immutable and content-named, so it is the wire format of the shared cache as it lies and can be shared, fetched and dropped whole. Relax's per-crate file is mutable. No change. |
| index: where, how found, cold pages a lookup | in each segment after its payloads; found by the manifest row's offset; interpolation on the key's first 8 bytes, then bisection; segments probed newest first, at most 16 | in the file's active slot; a reader copies the whole index at open | blaze for cold pages: a probe reads one or two pages of one run, where a copy reads the whole index. Changed: the lookup no longer reads each range's first and last entries (two cold pages a probed segment before), and the open no longer touches every index page. |
| write path and syncs | segments written with no lock and no sync; the flip under `flock` re-reads the head, appends the manifest, writes the inactive slot; no sync (durable mode: segment, log, head) | payloads appended under an exclusive lock, index written to the inactive slot, header flipped; one write barrier between payloads and header, left out at exit | blaze: no sync and no lock on the edit path; writers of several processes write in parallel and serialize only the flip (tens of µs). No change. |
| crash safety at each point; recovery | slots and manifests carry CRC-32C, segments BLAKE3; a slot whose manifest is not whole falls back to the other; segments no generation names are read from their footers and reported by name; the open guarantees the generation it read whole | ordering: payloads before the header by the barrier; a torn header invalidates the store; an exit's commit (no barrier) is verified against per-entry checksums before any reader serves it, else the commit before it wins | equal in what survives; blaze names every loss. Relax's exit tail is the better trade for after a restart: see the next row. |
| bytes verified when | this boot's segments trusted by their row; every segment of an earlier boot checked whole at the new boot's first open (every byte of the store read once a boot); fetched ones once at the fetch; a read checks nothing | readers never check; only the tail an exit left unbarriered is verified, once, then sealed | Relax. Adopted as the exit tail (b): after a command answers, `Driven::seal` puts one barrier on each segment this boot wrote and flips them sealed; the next boot proves a sealed row with no byte read and checks only the unsealed tail. |
| compaction: blocks? | slices on a lowered thread after the answer; the copy written with no lock; the flip replaces only what is still named; retired files unlinked after 10 minutes; never blocks a reader or a writer | rewrites the file whole as its next generation when the index outgrows its slot or dead bytes pass a quarter; under the writer's lock | blaze: a slice never blocks anyone and never copies a whole store. No change. |
| generations, renames | the head names a generation; segments are never renamed or rewritten; a reader keeps its mappings for its life; unlink waits a grace period; nothing is renamed over | a rewrite renames a temp file to a new generation's name, never over a file in use; lower generations pruned at the next start | equal: (c) already holds; the only rename is the solo leaf table's `.part`, read whole, harmless. No change. |
| checksums | CRC-32C per slot, manifest and record (read only by `scrub`), BLAKE3 per segment (the full check, the fetch) | a 64-bit hash per entry and per index, checked only for the exit tail | equal in strength where it counts; blaze's per-record CRCs cost nothing on a read. No change. |
| sharing: checkouts, machines | one store a user, every checkout reads it; namespaces (local, an organisation's, public); a trusted builder publishes whole segments with a signed statement | one store file per crate version under a target directory, shared by the programs of that target | blaze. Added (f): a put into a shared namespace is scanned for this machine's paths (`PlainConfig::machine`) and refused by name (`PutError::Machine`), before a record can reach a segment that could be published (rule 25). |
| range fetch from a static origin (R2) | an index is an immutable object a source serves by range (`find_ranged`: one 64 KiB read a lookup in a shard of a million, from the key space's ends); a segment fetched whole after its statement | none | blaze. No change here; the fetch itself is the remote lane's. |
| exit path | after the answer: compaction slices; nothing synced | the exit's commit leaves the barrier out and records where its payloads begin; the next open verifies them and seals them in the background | now equal: blaze's exit asks `seal` (barriers, then one flip), never waited on by the answer; a power cut before its flip leaves the tail unsealed and checked whole next boot. |

## What a repeat `check` reads from a cold page cache

A repeat with nothing changed, after a reboot, through the certificate (drive.md, bottom.md 9): the
head, the manifest, the certificate's lookup and payload, then the leaf table's stat sweep and the
slot file. Stats read no store page. Pages are 16 KiB on macOS, 4 KiB on Linux.

The floor is head + manifest + one index page + the payload + the leaf table + the slot: 6 pages
for hello-world, about 45 for reth (the leaf table is about 40 pages there). A sealed segment is
proved by its row, so no byte is read to prove it; the open touches no index page; a probed
segment costs 1 or 2 index pages, and the per-segment Bloom sidecar bound to the segment's digest
(`crates/blaze-cas/src/plain/bloom.rs`) skips a segment that cannot hold the key. These are counts
from the code paths, not a measurement. Not built: a test that counts the pages a repeat check
touches after a restart, on hello-world and reth.

## Built (crates/blaze-cas/src/plain)

- **(b) The exit tail, sealed.** Segment row flag `sealed` (store format 4 and later). `Plain::seal`
  (`Driven::seal`, asked after the answer by crates/blaze-cli/src/gc.rs) puts one `Effect::Barrier`
  on each local segment of this boot, an `Effect::Sync` on a fetched one, then one flip; a new
  boot proves a sealed row with no byte read and checks only the unsealed tail.
  - **The bound, decided** (2026-11-15). A sealed segment is not walked before the first answer
    after a boot; `Plain::scrub_next(budget)` walks it once a boot in the background, checking
    every record's CRC-32C, and a rot found is lost by name and retired by the flip. Built, not
    wired: nothing outside blaze-cas calls `scrub_next` (see "Still to build").
  - **The accepted risk.** A byte that rots in a sealed segment is found within one boot's
    background pass, or by `scrub`. A record read before its segment's pass that boot is
    unchecked, as this boot's own segments are.
  - **Not taken: a check on first read.** It needs a per-segment bitset that lock-free readers
    set, costs in proportion to what is read on the answer's path, and reverses decision 18
    (`Checks::per_read` stays asserted zero).
- **The open and the lookup.** The open touches no index page; `format::find_by` interpolates
  between bounds that narrow with each probe and reads no run ends (at most `3 + log2 n` probes);
  `format::find_many` seeks a sorted batch from where the last key ended.
- **(d) The startup image** (`plain/image.rs`, `StartupImage`, blaze-keyed `ReservedTag::Image`): the
  (slot, code record key) rows placed before `main`, in placement order, under
  `H("blaze startup image", manifest key)`; `read_order` sorts them by segment and offset. A hint:
  every record is still found by its own key. Built, not wired: blaze-run does not read or write it.
- **(f) Sealed sharing.** `PlainConfig::machine` holds needles (the store root, plus what the caller
  adds through `Driven::machine`, which crates/blaze-cli/src/store/plain_place.rs calls). A put or
  a commit into any namespace but local refuses a value that holds one (`PutError::Machine`).
- **(g) Puts on every worker** (`PlainStore`, plain/handle.rs). Each worker puts into its own lane;
  the only shared step is `commit_lanes`, which lays payloads and index in key order whatever the
  put order (one key with other bytes in two lanes is `TwoValues`, rule 21), writes, flips, then
  publishes. A worker sees its own put at once, every other reader after the flip. Still one
  segment per namespace per commit: per-worker files would add a probe per worker per lookup
  after a restart.
- **(h) What is compressed.** A record read on the edit loop is kept raw and read in place (rule
  51); compression is for what no build reads. The plain store compresses nothing.
- **D182's bottom** (`plain/bottom.rs`): see the last section.

## Left, and why

- **(a) A read-list digest per index entry.** blaze's is already as good. A record with reads is
  stored at a value key `H(key, the reads' digests)` (read-addressed.md, D39), so its validity is
  one lookup, the same index pages, with no field to compare. A 16-byte digest would grow every
  entry from 48 to 64 bytes: a third more index pages for every lookup, cold or warm. This
  disagrees with bottom.md 10's wording; the key form satisfies its intent.
- **(c) Never rename over.** Already true (see the generations row).
- **Relax's per-crate files, shared lock, whole-index copy.** Each is worse for blaze's goals: a
  copy reads the whole index, and a per-crate file is mutable and cannot be fetched by range as it lies.

## Still to build

- **The background pass in the helper.** In the helper left after the answer (rule 70, with its
  expiry), at idle IO priority: `Driven::scrub_next(SLICE_BYTES)` until it returns `None` (every
  sealed segment walked this boot) or the helper expires. A slice with `rot` carries a loss the
  profile records. The boot id passed as `PlainConfig::boot` is `kern.bootsessionuuid` on macOS
  and `/proc/sys/kernel/random/boot_id` on Linux, hashed to 64 bits.
- **blaze-run's `Parent` and the startup image.** At start, get `StartupImage::key_for(manifest)`;
  when found, run `read_order` against `locate` and read the records into the code region before
  the child's first ask (the Welcome's known slots come from the image's rows); after `main` is
  reached, put the image of what this run placed under `ReservedTag::Image` and `LOCAL` when it
  differs, committed with the run's other records after the answer.
- **A typed machine refusal at the commit.** The commit's refusal reaches the caller as
  `StoreError::Other` naming rule 25; a typed `StoreError::Machine` needs a row in blaze-keyed.
- **The read-ahead of a batch's ranges** (a range advice on blaze-core's mapping) and a cold
  wall-time run.

## The digest under every key

Every item, record and segment is named by BLAKE3 of its bytes (`crates/blaze-blake3`, our own;
`blaze-ids` wraps it). Most items are under 1 KiB, one BLAKE3 chunk, so the tree's lanes never
engaged on them. On 2026-11-15 the outline of the standard library hashed at 736 MB/s, and the
digests were 12.59 ms of a 17.78 ms edit to one item of a 10 MB file (blaze-file-items).

### The table

`examples/cost.rs`, one run, the owner's M5 Max (one P core, 4.25 GHz), best of seven, hot in
cache. SHA-256 and SHA-512 use the CPU's own instructions (ARMv8 SHA2 and ARMv8.2 SHA512). They
are a probe inside the example, checked against FIPS 180's examples; nothing in the tree calls
them. One input per call:

| bytes | BLAKE3 before | BLAKE3 now | official `blake3` crate | SHA-256 (hardware) | SHA-512 (hardware) |
|---|---|---|---|---|---|
| 16 | 61.4 ns | 46.6 ns | 47.2 ns | 26.0 ns | 148.2 ns |
| 64 | 63.2 ns, 1,012 MB/s | 46.6 ns, 1,373 MB/s | 1,357 MB/s | 38.6 ns, 1,660 MB/s | 443 MB/s |
| 256 | 1,017 MB/s | 1,324 MB/s | 1,367 MB/s | 2,810 MB/s | 671 MB/s |
| 1,024 | 1,014 MB/s | 1,306 MB/s | 1,362 MB/s | 3,141 MB/s | 924 MB/s |
| 4,096 | 953 MB/s | 1,224 MB/s | 2,444 MB/s | 3,207 MB/s | 1,027 MB/s |
| 65,536 | 767 MB/s | 2,542 MB/s | 2,504 MB/s | 3,266 MB/s | 1,063 MB/s |
| 16 MiB | 756 MB/s | 2,958 MB/s | 2,503 MB/s | 3,268 MB/s | 1,038 MB/s |

A tape of 65,536 inputs per call, in millions of inputs a second. "SHA-256 four" runs four
messages of one length together so that their rounds overlap, which is the fair rival of lanes:

| input bytes | BLAKE3 one at a time | BLAKE3 `hash_many` | SHA-256 one at a time | SHA-256 four |
|---|---|---|---|---|
| 16 | 20.9 | 42.8 | 33.5 | 49.8 |
| 64 | 21.3 | 41.6 | 24.8 | 29.9 |
| 256 | 5.07 | 12.0 | 10.6 | 12.5 |
| 1,024 | 1.26 | 3.10 | 3.05 | 3.86 |
| 1 to 1,024, log-uniform | 6.62 | 13.7 | 14.4 | not measured |

### What wins

- **Under 1 KiB, one at a time:** hardware SHA-256 wins, by 1.21x at 64 B and 2.41x at 1 KiB.
  One BLAKE3 input is a chain of compressions, 46 ns a block at its latency floor. BLAKE3 now
  stands at 1.01x that floor at 64 B and 1.06x at 1 KiB, so this gap stays.
- **Under 1 KiB, batched:** about even. BLAKE3 `hash_many` wins at 64 B by 1.39x, because
  SHA-256's padding makes 64 B two blocks. SHA-256 four wins at 1 KiB by 1.25x, at 16 B by
  1.17x and at 256 B by 1.04x. On mixed lengths, batched BLAKE3 is 5% behind SHA-256 one at a
  time.
- **Large blobs:** on one thread, SHA-256 leads by 1.10x at 16 MiB. That is not the whole
  comparison. BLAKE3's tree splits one blob across cores and across 16 AVX-512 lanes, and
  SHA-256 cannot split one blob at all. Neither the multi-core case nor x86-64 was measured
  here.
- **SHA-512** is out on this CPU: 144.5 ns at 64 B and 1,038 MB/s at 16 MiB.

### What a switch would touch, and what it would not gain

- Code: `blaze-blake3`, the one crate that computes the digest. `blaze-ids` (`b3.rs` wraps
  `hash`, `context_key`, `derive_key_with` and `Hasher`; `many.rs` builds `of_many` on
  `blaze_blake3::lanes`). `blaze-cond` (`Hasher::new_derive_key`). The 90 crates that depend on
  `blaze-ids` reach the digest only through `ContentId` and `b3`, so their source would not
  change. Other crates use the official `blake3` crate directly
  (the workspace's `blake3` dependency).
- Data: every content ID, key, record and segment name changes. Every home is rebuilt, and the
  shared cache and its precompiled std are filled again.
- Lost: BLAKE3's modes. `derive_key` gives each kind of ID its own domain, and SHA-256 would need
  a prefix block or a stored midstate to do the same. The keyed mode is a MAC in one pass, where
  SHA-256 needs HMAC's two because of length extension. The extended output goes too. So does
  the tree, which a range fetch from R2 could verify piece by piece.
- Not gained: the edit loop's digest share. Batching alone is 2.07x on the mixed tape (6.62 to
  13.7 M/s). Hardware SHA-256 adds at most another 1.25x, its best row. For the outline's
  12.59 ms that is a projection, not a measurement: about 6.1 ms batched, and about 4.9 ms with
  SHA-256 at its best.

### Keys must resist collisions

The shared cache is read by people who did not write it. A record is served to whoever asks for
its key. If someone who can choose inputs (a pull request that CI builds) could make two inputs
share a key, one input's record would be served for the other. So a key must be
collision-resistant. A non-cryptographic hash (xxh3, wyhash, CRC) is out for keys, however fast
it is. CRC-32C stays where it is: a checksum over bytes already named by BLAKE3.

### Recommendation

Keep BLAKE3 for keys, and batch the digests. The number that decides it: on the outline's mix of
lengths, batched BLAKE3 runs at 13.7 M inputs a second against hardware SHA-256's 14.4, a 5%
gap. SHA-256's best batched row is 1.25x ahead (1 KiB). Its worst is 1.39x behind (64 B). No row
reaches the 1.5x that would pay for rebuilding every record and losing the modes and the tree.
blaze-file-items hashes each file's items in one batch (`blaze_blake3::derive_key_many`, crates/blaze-file-items/src/digest.rs).
Revisit if a
measurement on x86-64 with SHA-NI and AVX-512, or of SHA-256 run four at a time over unequal
lengths, moves the mixed-length row by more than 1.5x.

## D182: the store's bottom, a seam (built, not the default)

Evidence (a one-function edit on tempo, the command's on-CPU time): `__open` and
`__open_nocancel` 9.5%, `read` 3.65%, stat calls 3.2%, the `read_all` copy 3.35%; the stored
answer is 29,972 pages (2.83 GB).

The store's driver is a `Bottom` (`crates/blaze-cas/src/plain/bottom.rs`, rule 78):
`OverDir<D>` hands every effect on as it is (today's behaviour, a pure refactor), and
`OverBlock<D>` makes every file of the store an extent of one device file, its table in one of
two alternating superblocks (generation, live extents, folders, digest). The free map is derived
at open, never stored. An extent a name drops is free only once the flip of the first generation
not naming it is durable, and never in the process that mapped it. A flip is the table taken,
the device synced, the slot written, the device synced. Flips run in order, and a queued flip
takes every later sync. A write grows within its extent, or moves the file whole to an extent
of twice the size. One process holds a device: its exclusive lock, refused by name if held, so
the two-writer mode stays on `OverDir`.

The names' calls are saved by the table alone: listing, rename, unlink, a folder's sync, the
temporary file of a durable write. The opens are saved only once the native driver keeps the
device's descriptor for every effect on its name; today it keeps descriptors for `ReadInto`
and `ReadMany` alone.

**Two writers on one device: a step, not a wall.** Accepted for now (coordinator,
2026-11-20): one process holds a device, and the two-writer mode stays on `OverDir`. Over a
block range it would need three things:

- extents claimed per writer in the head: each writer's run of the device named in the
  table, allocations only inside its own claim, a new claim taken by a flip;
- one owner of the flip at a time: the flip lock the store already takes (`Effect::Lock` on
  its flip file) taken on the device, the head read again under it, the writer's table merged
  into it, then written;
- what D159 makes of the store's one answer form: the merge is of disjoint claims, so it
  never rewrites another writer's extent. A writer that died leaves its claim, freed by the
  next open that finds no live holder.

**The descriptors kept for a held name: what removes the opens (not built).** The profile above puts
`__open` and `__open_nocancel` at 9.5% of an edit's CPU.

Where descriptors are kept today: `blaze-core/src/descriptors.rs` (`Descriptors::file`,
keyed by name and `Cache`, LRU-bounded at 1024 and a quarter of `RLIMIT_NOFILE`). Only
`Effect::ReadInto` and `Effect::ReadMany` read through it (`threads.rs` `run_kept`).
`threads::run` closes a name's descriptor after any effect that changes what is under the
name (`changes`: Remove, Rename, Write, Truncate, Clone, Link and the rest). Every other file
effect in `run_kept` opens its path and closes it: `ReadAll` (`std::fs::read`: open, fstat,
read, close), `Read`, `Append`, `Sync`, `Barrier`, `WriteAt`, `Map`, `Stat`. io_uring keeps its
own registered-file table for positional reads.

The rule a kept descriptor needs: its name changes only through this driver. That holds for the
store's content-named files, which is why reads may keep them. It does not hold for a source
file an editor replaces by rename. So holding is asked, never assumed:

- `Effect::Hold { file, access: Read | ReadWrite, tag }` opens once and keeps the descriptor
  outside the LRU, under its own cap (`HELD_MOST`, 256: the store's head, log, flip file and
  live segments; refused by name past it, never evicted). `Effect::Release { file, tag }`
  closes it.
- Every file effect on a held name runs on the kept descriptor:
  - `ReadAll` as `fstat` + `pread`, which saves the open and the close;
  - `Read`, `WriteAt` and `Append` as `pread` or `pwrite`, at the length the driver keeps for
    an append;
  - `Sync` as `fdatasync` and `Barrier` as `F_BARRIERFSYNC`;
  - `Map` as `mmap` of the kept descriptor;
  - `Stat` as `fstat`.
- An effect that changes what is under a held name (`Remove`, `Rename` onto it, `Write`,
  `Truncate` to zero, `Clone` or `Link` onto it) fails naming the hold (rule 3, no
  fallback); the asker releases the name first. `Truncate` to a length keeps the inode, so it
  runs on the descriptor.
- The simulation answers `Hold` and `Release` with a table of held names. It checks the same
  refusals, so a DST catches an asker that changes a held name. A crash releases every hold.

That fix needs no change of the store's format and helps `OverDir` today. The store holds its
segments as it maps them, its head and its log while open, and releases a segment before its
removal: `cleanup` already orders the removal after the flip. `OverBlock` holds its one
device. What it does not save: the opens of source files, which cannot be held. Their cost is
the stat-then-open of the incremental check, a separate piece.

The edits, in blaze-core:

- `lib.rs` `Effect`: `Hold`, `Release` and `HoldAccess`; `Done::Held` and `Done::Released`.
- `descriptors.rs`: a `held` map beside `kept`, with `hold`, `release` and `held(name) ->
  Option<Arc<File>>`.
- `threads.rs`:
  - `run_kept`: each arm listed above looks up `held(name)` first;
  - `changes`: on a held name, refuse instead of forget.
- `fs.rs`: descriptor forms of `read_all`, `sync`, `barrier`, `append`, `write_at` and `map`
  beside the path forms.
- `uring.rs`: `Hold` as a registered file (`IORING_REGISTER_FILES_UPDATE`).
- `sim.rs`: the held table and its refusals.

The store's core then holds and releases these names: `core.rs`, after its map of a segment,
at open, and before a removal.

Proof: `crates/blaze-cas/src/plain/bottom_tests.rs`, one crash simulation over both bottoms with
one oracle and a differential check between them, each with its plants.

The device under it is bstd's block grant (`crates/blaze-std/src/block.rs`): `Block`, `Lba`,
`Blocks`, aligned buffers and `DeviceFacts`, over `OverFile`, the simulated device
(`blaze-core/src/sim/block.rs`) and `RawDevice`. Making `Driven::open` use `OverBlock` is the
coordinator's call, after the tempo edit is measured on both.
