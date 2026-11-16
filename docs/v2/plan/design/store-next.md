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
- **(d) The startup image** (`plain/image.rs`, `StartupImage`, `IMAGE_TAG` = `ROOT_TAG - 1`).
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
3. At open: `Driven::machine(&[checkout, home])` in each spelling the command knows, before any put
   to a shared namespace.
4. The certificate: `locate` and one `pread` (`Driven::get_into`), not `get` (which faults the
   payload's pages on first touch).
5. Read `report().sealed` and `report().checked` into the profile: segments proved by row and
   checked whole this open.

blaze-solo: pass `Config::lanes` as the number of file threads the native driver was made with
(the binary passes 4, as kqueue runs).

blaze-run's `Parent`:

1. At start, get `StartupImage::key_for(manifest)`.
2. When found, run `read_order` against `locate` and read the records into the code region before
   the child's first ask. The Welcome's known slots come from the image's rows.
3. After `main` is reached, put the image of what this run placed under `IMAGE_TAG` and `LOCAL`,
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
- `a_lookup_reads_only_the_pages_its_probes_land_on`: page counts for a run of 200,000, against the
  old lookup as the control. `a_lookup_finds_every_key_of_an_uneven_run` (property): clustered
  prefixes.
- `a_record_naming_the_machine_never_reaches_a_shared_namespace`, with a store that lists no path
  as the control.
- `a_startup_image_round_trips_and_reads_in_place_order`.
- The format test now also refuses format 3 heads and manifests.
- blaze-solo: `a_sweep_asks_a_stamp_batch_per_file_thread`.
