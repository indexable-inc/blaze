---
status: plan
summary: blaze's plain store against Relax's store, row by row, for the two goals bottom.md puts first after the edit loop (a repeat after a restart that reads few pages; records fetched from a static origin by byte range). What was adopted on 2026-11-15 (the exit tail sealed, no index prefault, a lookup that reads no run ends, the startup image record, the sweep spread over file threads, the machine scan before a shared put), what was left and why, and the pages a cold repeat reads before and after.
date: 2026-11-15
---
# The store, next

The store is `crates/blaze-cas/src/plain` (its module doc is the format). The reference is Relax's
`compiler/src/store.rs` (MIT, read, not run). Goal order, from bottom.md 1: incremental compile
(after a restart too), then remote records, then memory, cold compile, disk cleanup, disk.

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

| step | today (before 2026-11-15) | after |
|---|---|---|
| head | 1 | 1 |
| manifest (80 bytes a segment row) | 1 | 1 |
| proving the segments | every byte of every segment of an earlier boot, at the boot's first open | none for sealed segments; the unsealed tail only (empty after an orderly exit) |
| open's index prefault | every index page of every segment | none |
| certificate lookup, a segment probed | 2 run ends + 1 or 2 probes | 1 or 2 probes |
| certificate payload | 1 | 1 |
| leaf table (whole, rows of path and stamp) | ⌈bytes ÷ 4 KiB⌉: 1 for hello-world, about 40 for reth | same |
| slot file | 1 | 1 |

The floor is head + manifest + one index page + the payload + the leaf table + the slot: 6 pages
for hello-world, about 45 for reth. After this change a repeat reads the floor plus 1 or 2 index pages
for each segment probed before the one that holds the certificate (newest first, at most 16). Today
it reads every byte of the store once a boot, plus every index page at each cold open. The ratio to
the floor is 1.0 when the certificate is in the newest segment, and at most about 6 (hello-world) or
1.7 (reth) when 15 segments are probed first. The remaining gap is the misses in other segments. The
piece that closes it is a per-segment fuse filter (blaze-ids' `Fuse8`, about 9 bits a key) in the
manifest row's segment, read with the manifest. That piece is not built (see "Left").

These are counts from the code paths, not a measurement: the driver does not read the certificate
through the plain store yet (blaze-solo opens the older `blaze_cas::Driven`).
`a_sealed_tail_survives_any_power_cut_and_is_never_read_to_prove` asserts the proving row: after
an orderly exit the next boot checks no segment in full.
`a_lookup_reads_only_the_pages_its_probes_land_on` asserts the lookup row against the old lookup
as a control.

## Adopted

- **(b) The exit tail, sealed.** Segment row flag 4, `sealed` (store format 4; format 3 is
  refused by name and rebuilt). `Plain::seal`, through `Driven::seal`, asks one `Effect::Barrier`
  (`F_BARRIERFSYNC`; Linux `fdatasync`) on each local segment of this boot not yet sealed. A
  published row gets an `Effect::Sync` instead: a fetched file is a whole-file write, which a crash
  may undo until a sync covers it. After the barriers comes one flip, `Change::Seal { (id, generation) }`.
  The flip seals a row only while it is still that row: a file written again gets the row's
  generation anew and is unsealed. A check at a new boot's open does not seal (only the barrier
  does), so a segment that only a check proved is checked again at the boot after.
  `Verified::sealed` is the third and last way a mapped segment becomes readable. A sealed row
  whose file length differs is lost by name.
  - **The bound, decided** (2026-11-15, by the coordinator, for the owner to overturn). A sealed
    segment is not walked before the first answer after a boot; it is walked once a boot in the
    background. `Plain::scrub_next(budget)` (`Driven::scrub_next`, on a lowered thread) walks the
    next sealed segment not yet walked this boot, oldest row first, at most `budget` bytes a
    slice, checking every record's CRC-32C. A whole walk sets the row's boot by a flip (the same
    record as the open's whole check: "walked this boot" is a row whose boot is this boot's id,
    a value passed in, rule 75). A rot found is lost by name, out of the lookup at once, and
    retired by the flip, through the open's own loss path. The next lookup misses, and the
    record is rebuilt or fetched.
  - **The accepted risk.** A byte that rots in a sealed segment is found within one boot's
    background pass, or by `scrub`. A record read before its segment's pass that boot is
    unchecked, as this boot's own segments are.
  - **Not taken: a check on first read.** Checking a record's CRC-32C the first time a process
    hands it out from a sealed segment needs a per-segment bitset that lock-free readers set,
    costs in proportion to what is read on the answer's path, and reverses decision 18
    (`Checks::per_read` stays asserted zero). The goal order puts the edit loop after a restart
    first; the pass costs it nothing.
- **No index prefault.** `on_map` no longer touches the index; `PlainConfig::page` is gone.
- **A lookup that reads no run ends.** `format::find` (now `find_by`) interpolates between bounds
  that start as the whole key space and become each probe's prefix as it narrows. Each guess stays
  inside the range, and the bound stays `3 + log2 n` probes.
- **(d) The startup image** (`plain/image.rs`, `StartupImage`, blaze-keyed `ReservedTag::Image`).
  - Its key is `H("blaze startup image", manifest key)`. Its body lists the (slot, code record
    key) rows placed before `main`, in placement order: `BLSI`, format 1, the manifest key, then
    rows of 40 bytes.
  - `decode_in` refuses another program's or format's bytes by a typed `ImageError`.
    `read_order` sorts what the store places by segment, then offset, so a cold read is one
    ascending pass.
  - It is a hint: every record is still found by its own key.
- **(e) The sweep on several threads.** `Config::lanes` (the driver's file threads) sizes the stamp
  batch at `ceil(rows ÷ lanes)` within 64..=4096. Before, every tree under 4096 files was swept by
  one kqueue file thread.
- **(f) Sealed sharing.** `PlainConfig::machine` holds needles (the store root by default; the
  caller adds more with `Driven::machine`).
  - `Plain::put` into any namespace but local refuses a value that holds a needle, with
    `PutError::Machine { key, namespace, at, needle }`.
  - `commit` scans a worker's shared batch the same way. That refusal reaches the caller as
    `StoreError::Other` naming rule 25, because blaze-keyed has no row for it; a typed
    `StoreError::Machine` there is the next step, for blaze-keyed's owner.
- **(g) Puts on every worker** (parallel.md change 4). `PlainStore` gives each worker a lane:
  one `Held` batch per namespace, behind a lock that only its worker and the commit take.
  `Store::put` uses the calling thread's lane; `PlainStore::lane` and `put_in` name lanes
  explicitly. A put reads the mapped segments (lock-free) and its own lane, and nothing else.
  Before, every put and every miss took the one writer lock. The only shared step is
  `commit_lanes`. Under the writer lock it swaps each lane's batches out (the lane's lock is
  held for the swap only) and lays them out as one segment per namespace. It writes and flips
  them, publishes the table, and only then lets go of the taken batches.
  - **Visible when.** A worker sees its own put at once. Every other reader sees it once its
    commit's flip has landed and the table is published, never before. A miss that races the
    commit's release asks the table again.
  - **One flight.** The driver's second asker waits on the first asker's task and takes its
    result in memory. That wait is on the task, not on the flip: a store read by the second
    asker before the flip misses, so blaze-drive must hand the value over, never re-read it.
  - **Equal bytes.** The commit lays payloads and index in key order, whatever the put order or
    lane (`seal_segment` too: one layout). One key in two lanes with equal bytes is laid once;
    with other bytes it is `TwoValues` (rule 21). Before, the layout followed put order, so two
    builders could write other bytes (rule 25).
  - **Crash.** A lane is memory only: puts not committed die with the process, and none was
    promised. A commit cut before its flip leaves a segment no generation names: the open's
    orphan pass reports and removes it. A commit cut after its flip leaves a named, unsealed
    segment: the tail the open walks. No per-worker file exists to be half-written.
  - **Segments at 128 workers.** Still one per namespace per commit, as parallel.md's
    state-of-the-art row decides: per-worker files would add 128 probes per lookup after a
    restart. Compaction and the page counts above are unchanged. The cost is one copy of each
    payload into the layout (the bytes are written then anyway) and one sort of the commit's
    entries.
  - Machine needles: the handle copies them at open and refuses at the put. The commit scans
    again with the writer's list, which `Driven::machine` may have grown.
- **(h) What is compressed, by kind (2026-11-15).** The rule: a record read on the edit loop is
  kept raw and read in place (rule 51); compression is for what no build reads. The plain store
  compresses nothing. The pack store (format 10):
  - **Output chunks:** raw. LZ4 is gone (and the crate dependency): a hit decoded each LZ4
    chunk, then copied it into the whole. Fixture (600 KiB text + 600 KiB noise): 5 copies
    before, 1 after; 643,274 bytes stored before, 1,229,694 (the bytes) after.
  - **Runs:** raw, now hot: a build reaching a run's dependents reads them, every process again.
  - **Debug info:** zstd with a per-kind dictionary from its put: read only on a panic or in a
    debugger, large and alike.
  - **Any other kind:** raw; a settle compresses one unread for two periods (zstd, or cold
    columns through blaze-format-tiers), and the end of the first period that reads it writes it
    back raw (was two periods running), so a read decodes until then, never on every read.
    Warm columns are read in place, so they keep the two periods.
  - A store of format 9 may hold LZ4 chunks: it is refused by name and rebuilt (rule 26).

- **(i) Batches.** `format::find_many` sorts a batch's keys and seeks each from where the last
  ended: one guess, a gallop to the key, bisection inside its last step. Index pages of a
  million entries (test `a_batch_touches_the_index_pages_its_keys_live_on`): 64 keys 171 one
  at a time, 87 batched, floor 63; 4,096 keys 2,776, 2,234, floor 2,186 (1.02). Not built yet:
  the read-ahead of a batch's ranges (needs a range advice on blaze-core's mapping, another
  lane's crate), the fuse filter (above), and a cold wall-time run.

## Left, and why

- **(a) A read-list digest per index entry.** blaze's is already as good. A record with reads is
  stored at a value key `H(key, the reads' digests)` (read-addressed.md, D39), so its validity is
  one lookup, the same index pages, with no field to compare. A 16-byte digest would grow every
  entry from 48 to 64 bytes: a third more index pages for every lookup, cold or warm. This
  disagrees with bottom.md 10's wording; the key form satisfies its intent.
- **(c) Never rename over.** Already true (see the generations row).
- **A per-segment fuse filter.** The next gap to the floor (above): it needs a manifest format
  change and a builder in the commit and compaction. Planned, not written here.
- **Relax's per-crate files, shared lock, whole-index copy.** Each is worse for blaze's goals: a
  copy reads the whole index, and a per-crate file is mutable and cannot be fetched by range as it lies.

## What the callers must do

blaze-drive (another lane's crate; nothing there edited):

1. After the answer, on a lowered thread: `Driven::seal()`, then `Driven::compact(n)`. Never before
   the answer, never waited on by it.
2. In the helper left after the answer (rule 70, with its expiry), at idle IO priority:
   `Driven::scrub_next(SLICE_BYTES)` until it returns `None` (every sealed segment walked this
   boot) or the helper expires. A slice with `rot` carries a loss the profile records. The boot
   id passed as `PlainConfig::boot` is `kern.bootsessionuuid` on macOS and
   `/proc/sys/kernel/random/boot_id` on Linux, hashed to 64 bits.
3. Puts from the workers through `PlainStore` (each thread its lane); `commit` once per batch.
   A one-flight waiter takes the first asker's value in memory, never by a store read.
4. At open: `Driven::machine(&[checkout, home])` in each spelling the command knows, before any put
   to a shared namespace.
5. The certificate: `locate` and one `pread` (`Driven::get_into`), not `get` (which faults the
   payload's pages on first touch).
6. Read `report().sealed` and `report().checked` into the profile: segments proved by row and
   checked whole this open.

blaze-solo: pass `Config::lanes` as the number of file threads the native driver was made with
(the binary passes 4, as kqueue runs).

blaze-run's `Parent`:

1. At start, get `StartupImage::key_for(manifest)`.
2. When found, run `read_order` against `locate` and read the records into the code region before
   the child's first ask. The Welcome's known slots come from the image's rows.
3. After `main` is reached, put the image of what this run placed under `ReservedTag::Image` and `LOCAL`,
   when it differs, and commit it with the run's other records after the answer.

## Tests (`crates/blaze-cas/src/plain/tests.rs`)

- `a_sealed_tail_survives_any_power_cut_and_is_never_read_to_prove` (property, on `Sim`).
  - Two writers commit. One of them seals or runs a compaction slice in alternate rounds, while a
    reader of boot 1 holds its maps.
  - Effects fail one in 40, and a power cut lands anywhere, inside the seal's barriers and flip
    too. Then always a crash and a new boot.
  - Oracle: the store's oracle (every completed commit reads back or is lost by name). Also: every
    segment proved by its seal passes `check_segment` on the disk's bytes; the boot-1 reader still
    reads what it held; and when the boot's last seal survived, the new boot checks nothing in full.
  - Control `the_oracle_catches_a_seal_before_its_barrier` (`Plant::SealBeforeBarrier`).
- `the_background_pass_retires_rot_and_resumes_after_a_cut` (property, on `Sim`).
  - Boot 1 seals one to four segments. One byte of one segment's last record rots. At boot 2 a
    holder maps a record of that segment, then durable processes walk one record a slice, and a
    power cut lands anywhere in the pass.
  - Oracle: no segment is walked again once its walk's flip landed, and none is skipped. The rot
    is lost by name and no lookup serves it. The holder's bytes stay. The store's oracle holds.
    Boot 3 has every survivor to walk again.
  - Control `the_oracle_catches_a_segment_marked_scrubbed_before_its_walk_ends`
    (`Plant::ScrubMarkedEarly`).
- `lanes_put_with_no_shared_lock_and_keep_every_flip_s_promise` (property, on `Sim`, durable).
  - One to eight lanes put their own keys, plus one key all of them put with equal bytes, in an
    order the seed turns. A reader's lane looks before and after each commit. One power cut
    lands anywhere in a commit (`Driven::cut_after`), then a new boot.
  - Oracle: a record whose flip landed reads back after every cut; none is seen by another lane
    before its flip; each worker sees its own put at once; no segment indexes a key twice.
  - Control `the_oracle_catches_a_lane_commit_flipped_before_its_bytes_are_durable`
    (`Plant::FlipBeforeSync`).
- `two_builders_on_other_lanes_write_equal_segments`: one lane against five, in reversed order,
  give equal bytes.
- `a_lookup_reads_only_the_pages_its_probes_land_on`: page counts for a run of 200,000, against the
  old lookup as the control. `a_lookup_finds_every_key_of_an_uneven_run` (property): clustered
  prefixes.
- `a_record_naming_the_machine_never_reaches_a_shared_namespace`, with a store that lists no path
  as the control.
- `a_startup_image_round_trips_and_reads_in_place_order`.
- The format test now also refuses format 3 heads and manifests.
- blaze-solo: `a_sweep_asks_a_stamp_batch_per_file_thread`.

## The digest under every key

Every item, record and segment is named by BLAKE3 of its bytes (`crates/blaze-blake3`, our own;
`blaze-ids` wraps it). Most items are under 1 KiB, one BLAKE3 chunk, so the tree's lanes never
engaged on them. On 2026-11-15 the outline of the standard library hashed at 736 MB/s, and the
digests were 12.59 ms of a 17.78 ms edit to one item of a 10 MB file (blaze-file-items).

### What was fixed in blaze-blake3

- The scalar compression read its message through a permutation applied each round. It now
  reads from a table of indices, every round written out: 49.4 ns a block from 63.6 ns.
- One input of a chunk or less is compressed where it lies, with its last block read by
  overlapping loads. Before, it was copied block by block into a buffer.
- The lanes were arrays left to the autovectorizer: 756 MB/s at 16 MiB, slower than one block at
  a time. They are now `core::simd`, eight lanes, with rotations by 8 and 16 done as one byte
  shuffle. The same code makes 16 lanes on AVX-512 and 8 on AVX2.
- New: `hash_many`, `keyed_hash_many` and `derive_key_many` hash many independent inputs, one
  input to a lane. A lane that finishes takes the next input on the next step, so inputs of
  different lengths need no sorting, no grouping and no buffer.

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
  change. `blaze-launch-host`, `blaze-launch-rt`, `blaze-codegen-code` and `ix-prof` use the
  official `blake3` crate directly. They are outside item keys.
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
The work is in the callers, not here: the outline and `blaze-items` should hash each file's items
in one `derive_key_many` (or `ContentId::of_many`) call, not one by one. One more step for the
coordinator: `blaze-ids`' `many.rs` duplicates `hash_many`, with its own grouping by block count
and its own `padded_words`, and could call `derive_key_many` instead (rule 30). Revisit if a
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

**The descriptors kept for a held name: what removes the opens.** Tonight's profile puts
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

The exact edits are in blaze-core, not this lane's files:

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

Proof: `bottom_tests.rs`. One crash simulation runs over both bottoms with one oracle:

- an acknowledged record reads back with equal bytes;
- a cut record reads back whole or not at all;
- a key never put is never found;
- the head after a cut is one of the last two generations.

A differential check compares the two bottoms without cuts. Three plants are each caught:

- both slots written in one step;
- an extent freed before the flip is durable;
- a stored free map trusted at open.

`Limits::SIMULATED` is checked against the drawn cases' high-water marks (D165), which the
test prints. `Limits::store` is not yet sized from a real store's marks.

The device under it is bstd's block grant (`crates/blaze-std/src/block.rs`): `Block`, `Lba`,
`Blocks`, aligned buffers and `DeviceFacts`, over `OverFile`, the simulated device
(`blaze-core/src/sim/block.rs`) and `RawDevice`. Making `Driven::open` use `OverBlock` is the
coordinator's call, after the tempo edit is measured on both.
