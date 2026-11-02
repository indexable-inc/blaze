---
status: plan
summary: blaze v2's store as written by the store lane: the files of a store directory, how a group commit survives a crash, hot and cold values, chunks, placing, verification, layers, and the remote tier.
date: 2026-10-29
---
# blaze v2: the store, as written

The design is the store lane file, [v2/plan/lanes/store.md](v2/plan/lanes/store.md); this is how `crates/blaze-cas` does it.
`Store` is a sans-IO state machine: it maps, writes, syncs and truncates through
`blaze_core::Effect`s and is told how each went by `Store::complete`.

## On disk

One flat directory per store (`Config::dir`):

| file | what |
|---|---|
| `head` | what the store is: its generation, which index slot is live (with its length, count and CRC-32C), each pack with its committed and indexed length, the dictionaries, the files of their own. Ends with a CRC-32C. Written whole with `Effect::Write` (temporary file, sync, rename, directory sync). |
| `pack-<n>` | the records of one crate version (#1298), of every kind, appended to only: a 16-byte header (magic, unit), then records. `n` is its place in the head's list, which it keeps when removed. Every commit ends it on a 4 KiB block, the rest of the block zeros, so every append can be written with `O_DIRECT`. A pack takes no appends once it is 1 GiB; its unit's next pack takes them. |
| `index-0`, `index-1` | the two index slots: an open-addressed table on keys, written whole into the slot the head does not name. |
| `head` packs | each pack's entry also holds its dead bytes (records since moved) and whether it is removed; a removed pack keeps its place in the list so the numbers records name stay put. |
| `file-<content>` | a hot value of 4 MiB or more (not an output), a file of its own: mapped on its own, placed by cloning. |

Packs sit next to the head so the directory sync of the head's write makes a new pack's name
durable with it.

A **record** is a 96-byte header (magic, kind, form, a CRC-32C of the header, dictionary,
stored and raw lengths, key, content hash), the payload, and zeros to the next 16 bytes. Its
form is `Raw` (read in place), `Zstd`, `Lz4` (a chunk), `Chunked` (a list of chunk keys and
lengths), `External` (the value is `file-<content>`), `Dictionary`, `Warm` or `ColdColumns`
(a value's columns, below: the payload is the schema's 32-byte hash, then the encoded bytes,
so warm bytes keep the record's 16-byte alignment). The store's format is 2. `Entry::content` is BLAKE3 of the
value's bytes under `"blaze v2 store value 1"`.

An **index table** (`index.rs`) is laid out as a SwissTable: a 64-byte header, a control
column of one byte a slot (empty, or 7 bits of the key), then a 64-byte row a slot (the key, the
pack, kind, form, offset, stored and raw length), `64 + 65 * slots` bytes. A lookup probes groups
of 16 control bytes at triangular steps from the key's first 8 bytes (a BLAKE3 hash is uniform),
matching the tag in all 16 at once (SSE2, NEON, or two words without carries), and reads a row
only on a tag match. At most 7/8 full: a hit is a quarter line of control bytes and one row. The
table is built from the sorted set, so its bytes depend on the set alone; it is checked whole
when mapped and not again.

## A group commit (#1298)

Reads and writes take no lock ([v2/plan/lanes/concurrency/store.md](v2/plan/lanes/concurrency/store.md)).
A `Reader` (`Store::reader()`, `Send + Sync + Clone`) reads through the published view with the
task's guard. A worker puts through a `Writer` of its own (`Store::writer()`; `Store::put` is
the owner's): it encodes the record into its own segment's chunk and publishes a spot (key,
content, place, ticket) into the delta with one compare-and-swap, so the value is readable at
once and has a ticket, `(segment << 40) | put`. `flush` starts one commit for everything put
since the last, by the store and by every writer: it drains every segment into pack memory, then

1. each pack written to is cut back to its committed length (`Truncate`), then written at that
   length (`WriteAt`, block aligned, so a driver may skip the page cache), then synced (`Sync`,
   asked only once the write has completed);
2. new files of their own are written (`Write`); and once the records the index table does
   not hold reach 65,536, a new table of all of them goes into the inactive slot (`Write`);
3. once all of those are durable, the head is written, naming the new lengths and slot.

Before step 1 each pack written to is padded with zeros to the next 4 KiB block, so the write
starts and ends on a block; v2/requests/store.md asks the io_uring driver to write packs with
`O_DIRECT` from registered buffers.

The head's write is the flip: a crash before it leaves the old head, naming only bytes that were
whole before; after it, everything it names is durable. Bytes past a pack's committed length
(a torn write) are never read and are cut by the next commit. Any IO failure fails the whole
commit, which starts again from the committed lengths; eight failures in a row break the store
(`Store::broken`), which then serves what it has mapped and makes nothing durable. 77,481 puts
are one commit: a few large writes, a sync per pack, one head.

After the flip the store maps each grown pack again; once a batch's packs are mapped, each spot
is retargeted to its pack with one swap and the segments' chunks are retired. A mapping is never
read past the length it was asked for.

## Opening (#686)

The store reads `head`, maps the live index slot, every pack and every file of its own, then
checks the index table's CRC-32C and reads the headers of the records between each pack's
indexed and committed lengths into an in-memory delta, stepping over the zeros that end a
commit. Nothing else is read: the index is the mapped table. Only the daemon holds a
store (one writer: the commit cuts packs back to their committed length, so two writers would
corrupt each other; v2/requests/store.md asks for the lock that enforces it).

A head of another format, a pack shorter than its head says, an index that does not match its
CRC, a record header in the committed range that does not check, or anything but zeros where a
commit's padding is stops the store, naming the file. Nothing is read as
another format and nothing is skipped.

## Hot and cold

Query results, compiled code, outputs and the image are **hot**: kept raw and read in place. Debug
info and build-script runs are **cold**: read rarely, kept zstd (level 6) with a dictionary of
their kind. A cold kind's first values are kept raw; once 256 of them (2 MiB) are there, a
64 KiB dictionary is trained on them, written as a record and listed in the head, and the kind's
values from then on are compressed with it. A training that fails is in `Store::problems` and is
tried again on twice the samples. A cold value of 8 MiB or more gets no dictionary but zstd's
long-distance matching over a 128 MiB window, on 4 threads: a fixed count, so the bytes are the
same on every machine. zstd refusing a value stops the store; it is never kept another way. A
compressed value is decompressed on read and held until the store is next changed. Each
dictionary is digested once when it is read or trained (zstd's `CDict` and `DDict`, shared by
every thread), so a put or a read of a cold value loads no dictionary.

## Chunks and placing

An output of 256 KiB or more is split by content (FastCDC, 16/64/256 KiB, a fixed gear table):
each chunk is a value keyed by the hash of its bytes, kept once however many outputs hold it,
and the output a list of them. A chunk is kept in LZ4 when that saves an eighth of it, raw
otherwise: a chunk is copied out when its output is assembled, so decompressing it costs about
the copy. Two builds of a binary that differ in a few functions share
all but the chunks around the differences.

`Store::place` puts a value at a path outside the store. A file of its own is cloned
(`Effect::Clone`: `FICLONE`, `clonefile`), after removing what is at the path; anything else,
chunked outputs included, is written whole, copied with non-temporal stores from 4 MiB on (the
copy goes to the kernel, not back to the store). The form decides, never a try: where blocks cannot
be shared the clone fails, naming the file system, and nothing copies instead.

## Verified

`get` checks the record header (its CRC-32C and its key), then hashes the value's bytes against
its content hash before handing them out, once a record (the bytes of a mapping or of memory at
a place never change). A chunked value checks each chunk and the whole. A value that fails is
`Corrupt` and never served; `verify` checks every value afresh, with the index tables' CRCs and
the dictionaries. CRC-32C finds torn and rotted headers and tables; what a value's bytes are
is always checked by BLAKE3. Places are `FileName`s, which cannot leave the driver's root, and `place`
refuses a path in the store.

## Keys (#1439, #1480)

A key is the caller's: the store adds nothing to it, so no key hashes all of blaze's sources and
a release that changes nothing a value depends on keeps it. `Key::of(schema, producer, inputs)`
is the one way to make one: the schema of the value, the content ID of the code that made it
(its own dependency closure), and every input. A put of a key with other bytes than it keeps is a
`Mismatch`, which is a key that misses an input. Build-script runs are keyed by their traced
inputs (`blaze-usercode`'s `RunKey`), which take in the C compiler they ran.

## Layers

`Config::layers` are read-only stores searched after the store's own: the shared base layer of
public dependencies. They are opened as the store is and never written; a put of a key a layer
has is durable at once. A layer that is not there stops the store.

## A shared store

`Store::open_shared` opens a store several daemons on one machine write at once: one build's
std under `<user cache>/blaze/std/<blaze build id>/store`, shared by every BLAZE_HOME. No lock
is taken. Each daemon is a writer (`SharedConfig::writer`, one per BLAZE_HOME) with a store
directory of its own in the shared one, `w-<writer>`, written as any store is, so two daemons'
commits never touch one file. Every other writer's directory is read as a layer behind its own,
as of its last look: a get that misses calls `Store::look_again`, which lists the shared
directory, reads each writer's head again, and maps a writer whose head changed afresh,
checking it whole before readers see it (until then they read it as it was), and gets again
once `Store::looked` reaches the number it returned. A put of a key another writer's commit
holds keeps nothing, so equal values are one entry; two writers that put one key before either
has seen the other each keep a copy, which reads the same. Every value is checked against its
hash wherever it is read. `Effect::Private` is asked of the whole shared directory, and a writer's
store that cannot be read once its head has been read again `FAILURES_MOST` times stops the store
naming the file; there is no other store it reads instead.

## The remote tier

`remote::Server` serves a VM daemon's store over a Unix socket or a loopback port (forwarded over
SSH); `remote::Client` is a laptop daemon's way to it. `Store::fetch` asks; the answer is checked
against its content hash and put like any value. The channel is what is trusted for the key; the
bytes are checked. It is never on the VM's hot path: the VM's builds read their own store.

## One file per crate version (#1298)

A **unit** is a crate version as the store numbers it: `Store::unit(crate_version)` gives the
same `Unit` for the same content ID, and the head lists the units in order, so the numbering
survives an open. `Store::put_in(unit, ...)` appends to that unit's pack, so one crate version's
query results, code and debug info sit in one mapped file. A crate's build reads them together,
and a crate version's records can be dropped as a whole. Chunks of outputs, dictionaries and
values put with `Store::put` are in `Unit::SHARED` (0), since no one crate version owns them. A
record moved between tiers, or copied out of a retired pack, stays in its unit. A commit
appends to each pack written, syncs each and then writes the head once: the barrier is the
head's write, whatever the number of crate versions.

## Hardware (plan/design/hardware.md)

Each hot loop picks its path at run time by CPU feature, and every path gives the same bits, so
which one runs is dispatch, never a fallback:

| loop | paths |
|---|---|
| CRC-32C (`crc.rs`) | SSE4.2 `crc32` or ARMv8 CRC, three streams for long inputs; tables elsewhere |
| FastCDC gear hash (`chunk.rs`) | AVX-512 (8 positions a step: gather, a shift-and-add scan, a lane test) or NEON; scalar elsewhere |
| copies to the kernel or the network (`copy.rs`) | non-temporal stores (AVX-512, SSE2, `stnp`) from 4 MiB on |
| BLAKE3 | the `blake3` crate's run-time AVX-512 / AVX2 / SSE4.1; NEON and many-small-inputs batching are requests to format |

The differential tests run every path the test machine has against the portable one. What the
store asks of the drivers (`O_DIRECT`, registered buffers, SQPOLL, linked submissions, huge pages
and NUMA-local mappings) is in v2/requests/store.md; threads and pinning are core's.

## Settling: hot, warm and cold by reads

`Store::settle` is called by the daemon when it is idle. Each call is a period: what was read
and what was made in it and in the one before is known from four bit columns (`flags.rs`). It
moves records between tiers by how recently they were read, copying each into a new record and
leaving the old one dead. A record of 4 KiB or more moves a tier colder once it was neither read
nor made in the last two periods:

- a value of a schema the store was given `BufferColumns` for (`Store::encode_columns`) goes from hot
  to warm columns, or straight to cold where warm saves less than an eighth, and from warm to
  cold columns;
- any other raw or LZ4 record goes to zstd, batched by kind with that kind's dictionary (trained
  first from the kind's records if it has none);
- a record whose next tier does not save an eighth stays and is marked tried, never tried
  again;
- a zstd, warm or cold record read in two periods running comes back raw (an output, LZ4).
- a sealed pack at least half dead is retired: its live records are copied on, the next head
  marks it removed, and once that head is durable its file is removed. A pack named removed
  when the store opens is removed again.

A call does at most 256 MiB and says if there is more. Every move round-trips the same bytes,
checked against the content hash before it is copied. What was read is not kept across an open
or an index rewrite (rows are renumbered), so after either nothing goes colder until two periods
have passed.

## Columns: what each tier is (plan/design/data.md, "Compression follows the columns")

What a value's columns are is format's; when a value is in which tier is the store's. The store
is given a `Columns` per schema: four functions from the value as put (a `blaze_format` buffer,
whose header names the schema) to its warm form (bit-packed by frame of reference, random
access, read in place) and its cold form (a codec per column, then zstd), and back. Every
encoding the store makes is decoded and compared with the value before it is kept; one that
does not give the value back is not kept, is in `Store::problems`, and the record is marked
tried. A cold kind's value of such a schema is put in cold columns at once.

`Store::get` always gives the value as it was put, decoding warm and cold records (held until
the store next changes, as zstd's are). `Store::get_in_place` gives a warm record's bytes where
they are, with `Heat::Warm`, for readers of the schema's warm form. Either is checked the same
way: decoded and hashed against the content hash, once a record. A record in columns of a
schema the store was not given is refused, naming the schema; nothing reads it another way.

The proof's column shapes (`blaze_v2_proof::columns`: empty, runs, sorted, CSR offsets, each
narrow range, a frame far from zero, the extremes) are run through the store itself. Each shape
is put as a value of columns, moved by `settle` as cold as it goes, read back hot, and
reopened, and must read the same at every step (`columns::round_trips`). What the first move
leaves, warm or hot, must read element by element in place (`columns::reads_in_place`).

## Data-oriented (plan/design/data.md)

- Per-record flags (checked, read and made this period and the one before, compression tried)
  are bits in words beside each table's slots (the index's and the delta's), not sets of keys:
  a period's turn is a move of one column into the other. A flag is set with one relaxed
  `fetch_or` only when not set, so a read takes no lock, and a record read again writes no
  shared cache line.
- Maps on keys and content IDs (the delta, files of their own, fetches) index by the BLAKE3
  hash's first eight bytes: a lookup hashes nothing.
- The index's control column is the dense array probing runs over; the plan's row asks for keys
  and places in separate arrays, but a hit needs both, so they share one 64-byte row: one miss a
  hit, not two.
- Settling batches by kind: one compressor and dictionary a kind, records in pack order.
- Size budgets are const-asserted: `Key` 32 bytes, `Loc` 32 (a place in the delta and the
  index), an index row 64,
  the head's fixed part 72 and a pack entry 32.
- Column codecs (frame of reference, run lengths, deltas over a column) are format's, reached
  through `Columns`; the store keeps opaque values as before, raw or zstd.

## Not done here

- Eviction: no live record is removed from a store; settling removes only dead space. The VM's
  disk is taken as unlimited (plan/design/model.md); a store is removed whole.
