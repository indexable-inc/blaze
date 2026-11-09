---
status: plan
summary: The store's hot tier as a vmcache-style buffer pool - one reserved range with a fixed slot per page of every store file, a state word per page (optimistic and pinned reads), misses as explicit reads through blaze-core that the task yields on, immutable records written once through the pool, eviction by the one evictor with MADV_DONTNEED / MADV_FREE_REUSABLE, prefetch we plan ourselves (change set, query graph, learned page sets, per-view warm sets), codegen outputs then memos then the image on it, a DST, and the lanes.
date: 2026-11-09
---
# The store's buffer pool

What a task makes and something else reads later (a compiled function, a unit's object code, a
query's result) is a blaze-format record. It is written once into the store and read in place
(rule `outputs-are-records`, the `memory-and-data` skill). This doc designs the part
that reads it in place: the store's **hot tier**, a buffer pool of blaze's own, in the style of
vmcache. It is step 4 of [memory-footprint.md](../../../memory-footprint.md#4-outputs-as-records-in-the-stores-buffer-pool).
The pool is the first cache that [memory-pool.md](../../../memory-pool.md) puts over the store and
under the one evictor. It builds on admission by footprint (docs/v2/plan/admission-by-footprint.md on branches
admit-footprint-1106 and kept-memory-1109, not yet on main). It does not redo that work: admission by footprint, the purge before a refusal,
and naming each task's crate and unit are that lane's. This design adds one input to that lane's
gate, and one step.

Prior art: Leis, Alhomssi, Ziegler, Loeck and Dietrich, "Virtual-Memory Assisted Buffer Management"
(vmcache, SIGMOD 2023); Crotty, Leis and Pavlo, "Are You Sure You Want to Use MMAP in Your Database
Management System?" (CIDR 2022); for prefetch and warm restarts, InnoDB's buffer pool dump and load
and PostgreSQL's `pg_prewarm` autoprewarm.

## What it replaces, and why

Today the store maps every file it reads in place. It maps each pack (`PackView::mapping`, `Pack::mapping`
in `crates/blaze-cas/src/read.rs` and `lib.rs`), each index slot (`Index::mapping`), and each file of
its own (`ExternalView::mapping`), all through `Effect::Map` and `crates/blaze-core/src/fs/mapped.rs`.
Committed bytes wait in `Pack::memory` until a fresh mapping covers them, and the store maps a
pack again after every commit. `Pinned` holds a value past its task by cloning the mapping. The
query database writes cold values to files of its own and maps them (`Pending::ColdWrite` and
`Pending::ColdMap` in `crates/blaze-query/src/lib.rs`). The daemon image is one mapping
(`crates/blaze-query/src/image.rs`, `crates/blaze-image`).

The mapping is not where the memory went. On macOS a clean file page is not in the footprint at all.
The memory went to outputs kept on the heap until something serialized them: codegen's heap peaked
at 13.67 GiB, and query storage grew to 15.3 GiB and then to 31.5 GiB
([memory-footprint.md](../../../memory-footprint.md#the-causes)). They stayed on the heap because
the store could only serve what was already committed and mapped. So "write it once, read it in
place" was never the fast path, and nothing owned the hot set. The pool makes it the fast path, with
an owner. It also takes out the costs of mapping that the CIDR paper lists, each of which blaze has
met:

- **A miss blocks a worker thread.** A fault on a page not in memory stops the thread in the kernel,
  and no fiber can run on it meanwhile (rule `deadline-waits`: a fiber parks on the scheduler,
  never in the kernel).
- **The kernel decides what stays.** Eviction and read-ahead are the page cache's. blaze can neither
  rank them nor count them, and on Linux the page cache is charged to the daemon's cgroup.
- **A failed read is SIGBUS.** It is not an error for one task (rule `panics-fail-one-job`). The
  module comment of `fs/mapped.rs` says blaze installs no handler, so the whole daemon stops.
- **The process's `mmap_lock`.** Every map, unmap and fault takes it. On 2026-11-05 a cold
  `blaze check` spent 99.7% of its CPU in the fault handler behind those writers (the module
  comment of `crates/blaze-mem/src/os.rs`). Remapping after each commit is another writer.
- **No prefetch blaze controls** (section 5).

## 1. Layout

### One reservation, a window per file, a slot per page

The pool is one range of address space reserved at the store's open:
`Region::reserve_sparse(bytes, Tag::Pool)` (`crates/blaze-mem/src/region.rs`). On Linux and macOS
every byte of that region is readable and writable from the start (`MAP_NORESERVE`), and only
touched pages cost memory. It is advised not to use transparent huge pages, because dropping 64 KiB
inside a huge page would split it. `Tag::Pool` is a new tag. `Tag::Store` keeps counting the
store's tables and segments, so the counters tell the pool apart from the rest of the store.

The range is carved into **windows** of `WINDOW = PACK_MOST = 1 GiB` each. Every store file that is
read in place gets one window when it opens or is made, or as many consecutive windows as its length
needs. That covers the packs, the two index slots, and the files of their own of 4 MiB or more
(`EXTERNAL_FROM`). A record longer than a pack's limit has a pack of its own, and so a run of
windows. `pool::Windows` is the table from a file to its first window. It is a dense `Vec`
indexed by pack number, plus entries for the externals and the index slots. A retired pack gives its
windows back to a free list once its file is removed.

A file's byte `at` lives at `base + window * WINDOW + at`. So page `n` of a file is at
`base + window * WINDOW + n * PAGE`, and finding it is arithmetic plus one state word. An evicted
page comes back at the same address, so a `Rel` or `Slice` in a record means the same after a
reload.

**Pages are `PAGE = 64 KiB`.** That is 16 base pages on Linux, 4 on macOS arm64 (16 KiB pages),
the image's default group (`blaze_image::DEFAULT_GROUP_SHIFT = 16`), and a size an NVMe drive reads
at full speed in one command. Smaller pages would multiply state words and reads. Larger ones would
read more bytes nobody asked for, since most query values are a few hundred bytes to a few KiB.

**A record larger than a page lies across consecutive pages of its file's window.** It is one
contiguous slice in memory with no copy and no stitching, because the window is contiguous. A
**run** is the range of pages a record covers: `at / PAGE ..= (at + len - 1) / PAGE`. A miss loads
the run's unloaded pages in as few reads as there are gaps. A 40 MiB object (a file of its own) is
one window and 640 pages, and loading it whole costs one read of up to 1 MiB per command. There is
one page size. A second size (2 MiB pages for files of their own) is an open question, decided by
the measurement in section 7.

Size of the reservation: 4 TiB by default, which is 4096 windows. The store's open checks it
against the head (live packs plus externals plus the two index slots, with room to double). A store
that needs more stops, with an error that names the store, its live files and the reservation
(rule `no-fallback`). macOS grants a reservation that large to a user process (decided below: blaze
already reserves thousands of GiB there), and lane 1's reservation test keeps it so; were it ever
refused, the fallback-free answer is a smaller default and the same error.

### The page state word

The state words are a second sparse region, one `AtomicU64` per page of the reservation. That is
8 bytes per 64 KiB, one part in 8192, and only the words of touched windows cost memory.
`pool::Word` packs:

| bits | field | meaning |
|---|---|---|
| 0..20 | `pins` | pinned readers of the page (up to about a million; a pin that would overflow is a bug, and panics naming the page) |
| 20..23 | `state` | `Unloaded`, `Loading`, `Loaded`, `Evicting`, `Corrupt` |
| 23 | `referenced` | set by a pin, cleared by the evictor's sweep (CLOCK's second chance) |
| 24 | `prefetched` | loaded by a prefetch and not yet pinned by a demand read (section 5) |
| 25..64 | `version` | 39 bits, raised by every eviction; an optimistic read checks it |

The transitions, each one compare-and-swap of the word:

- `Unloaded -> Loading`: the one asker that wins the CAS loads the page. Everyone else waits on
  its load.
- `Loading -> Loaded`: the loader, after the read completed and its block checks passed
  (section 2). The loader's own pin is set in the same CAS.
- `Loading -> Unloaded`: a read failed. Its waiters wake to a typed error, and nothing is kept.
- `Loading -> Corrupt`: a block check failed. The page is refused every time it is read,
  until its pack is retired.
- `Loaded{pins: 0} -> Evicting{version + 1}`: the evictor. After this no pin can succeed, and
  every optimistic read that began before it fails its check.
- `Evicting -> Unloaded`: the evictor, once the pages are given back to the kernel.

`Evicting` exists so that a load cannot start on a page whose bytes the evictor is about to
drop. With only `Loaded -> Unloaded` before the `madvise`, a loader could win the page and finish
its read, and then the evictor's `madvise` would drop the bytes it had just read. That race is
planted bug 3 of the DST (section 7). An asker that finds `Evicting` waits for the evictor's batch,
which takes microseconds, through the bounded spin-then-park helper.

Records are immutable, so no page is ever dirty, and no state means "being written by a reader".
Section 3 covers the one writer, the commit.

### Reading: optimistic and pinned

**Pinned** is what hands out a reference. `Pool::pin_run(run)` adds one to `pins` of each page of
the run, with one CAS each, from `Loaded`, setting `referenced` in the same CAS. While any pin
is held, the evictor cannot take the page, so the bytes stay. The guard `Pinned` takes the pins back
on drop with one `fetch_sub` each. A pin is an atomic count, not a thread's slot, so it may be held
across a fiber wait (rule `no-wait-across-thread-locals`) and moved to another thread.

A hot page that many cores pin would turn its word into a contended line. So a task keeps a
**`PinSet`** in its frame: the runs it pinned, at most 32, as a small array. A second read of
the same page in the same task finds it there and touches no shared word. The set lets its pins go
when the task ends, or when the task resets its frame between items (memory-footprint.md step 3),
whichever comes first. A value held past that is pinned on its own, as a `Pinned` guard the
holder owns.

**Optimistic** reads copy bytes out and never hand out a reference. They are for the store's own
small reads: an index probe's 16 control bytes and 64-byte row, a 96-byte record header, the
block checks. `Pool::read_optimistic(at, out: &mut [u8])` does the following:

1. Load the word with Acquire. It must be `Loaded`, else this is a miss.
2. Copy the bytes with `blaze_mem::racy::copy_out`, word-sized relaxed atomic loads. These are
   defined behavior while the kernel drops the page underneath. A plain `&[u8]` read would not be.
3. Fence with Acquire, then load the word again.
4. The copy is good only if the state is still `Loaded` and the version is the same.

This is the seqlock that `MemoHot::inline` already uses (`crates/blaze-query/src/memo.rs`). An
optimistic read writes no shared line, so a lookup that hits, which the store's rule says must
write nothing, still writes nothing.

## 2. Reads

### The API

The store's reader gains one entry point, and `Pinned` changes what it holds:

```rust
impl Reader {
    /// The value of `key`, verified, its pages pinned: loads what is not in memory,
    /// the calling task yielding until it is, never blocking the worker.
    pub fn pin(&self, key: &Key) -> Result<Option<Pinned>, ReadError>;
    /// As `pin`, into the task's `PinSet`: the bytes are the task's until it ends or resets.
    pub fn read<'t>(&self, task: &'t PinSet, key: &Key) -> Result<Option<Entry<'t>>, ReadError>;
}

/// A record's pages pinned (a run, or a value made whole in the `Owned` table).
pub struct Pinned { key: Key, kind: Kind, content: ContentId, run: Run, bytes: NonNull<[u8]> }
impl Pinned {
    pub fn get<P: Persist>(&self) -> Result<P::Ref<'_>, FormatError>; // P::read of the bytes
}
impl Deref for Pinned { type Target = [u8]; }
```

`Persist::read(&[u8])` (`crates/blaze-format/src/lib.rs`) already reads a value in place from
any aligned slice. A pinned run is one: records are 16-byte aligned in their pack and pages are
aligned to 64 KiB. So nothing in blaze-format changes. A `P::Ref<'_>` borrows the guard, and the
borrow checker holds the pins for as long as the reference lives.

The guard lives as long as its holder needs the value, with these rules:

- A query run reads through the task's `PinSet`.
- A codegen unit or a link that holds inputs for minutes holds them as `Pinned` guards.
- Pinned bytes are not evictable. `Pool::pinned_bytes` is counted, and admission charges it like
  any live bytes (section 4). A holder that pins more than it needs shows up in the counters, not
  as a mystery.

The `Pinned` the store hands out today (a clone of a `Mapping`, or an `Arc<Aligned>` copy) is the
same concept, so it keeps its name and its one home (rule `one-concept-one-home`). Its
`PinnedBytes::Mapped` goes. Values made whole (decompressed cold values, chunked outputs put
together, warm columns decoded) stay in the store's `Owned` table, as today, and a `Pinned`
of one holds that table's entry.

`Pinned::file` stays for `Kind::Machine` alone. Wasmtime maps a module's code to run it, copy on
write. That mapping is a loader's, of a file that is never read in place as a record. It is the one
mapping of a store file that remains, and it is not the hot tier.

### A miss

A miss is an explicit read that the task yields on:

1. `Reader::pin` finds the key's `Loc` (pack, offset, stored length) through the index and the
   delta, as today. Index pages are read optimistically through the pool. A miss on an index page
   is a miss like any other.
2. It computes the run and tries to pin each page. A page in `Unloaded` that it wins with
   `Unloaded -> Loading` is its own to load. A page another asker is loading, it waits on.
3. It hands its pages to the **loads core**, `pool::Loads` (`crates/blaze-cas/src/pool/loads.rs`,
   `#![blaze::sans_io]`). The core coalesces adjacent pages into one read and keeps a fixed table
   of `LOADS = 4096` loads in flight, each with its reusable event. It hands back
   `Effect::ReadInto` effects, and takes `Completion`s.
4. The task waits on the load's event, `blaze_graph::sched::host::Event`. That is the scheduler's
   one-shot key that any thread may fire, a driver's or an IO thread's. A fiber that waits on it
   switches out and holds no thread. A thread on no fiber parks. The wait runs inside
   `blaze_graph::wait::wait(WaitOn::Io("store pack-17 pages 1200..1216"), ..)`, so the profile says
   what it waited for (rule `every-wait-says-what`).
5. `Event` claims a fresh key and a scheduler each time it is made. So the loads core keeps one
   per load slot, and a new `Event::rearm` claims the slot's next key in the same scheduler. That is
   one allocation per slot, not one per miss.
6. A full table is a wait too: the asker waits on the core's "a slot is free" event, named
   `io store pool: loads full`. It is bounded: every load ends, completed or failed.

**The loader is a thread of the pool's own**, `pool::drive` (`crates/blaze-cas/src/pool/drive.rs`).
It is thin: it drains asks from a `crossbeam::queue::ArrayQueue`, steps the core, submits its
effects to a blaze-core driver of its own, polls completions with a deadline (`Driver::poll`,
`blaze_core::wait`), and fires the events. An ask wakes it through its driver's own wake: the
`EVFILT_USER` trigger of `crates/blaze-core/src/kqueue.rs` on macOS, an eventfd read on the ring
on Linux.

- **Linux**: one io_uring (`crates/blaze-core/src/uring.rs`), `SINGLE_ISSUER | DEFER_TASKRUN`, the
  pool's files opened once, `O_DIRECT`, in its fixed file table. A read is `IORING_OP_READ` into
  the slot's address. With `O_DIRECT` the bytes are not also in the page cache, which would charge
  them to the cgroup a second time.
- **macOS**: the kqueue driver's file threads (`kqueue.rs`, `threads.rs`). The pool's driver has
  its own threads, `2 × cores` of them, so an NVMe drive sees a queue deep enough. Each does a
  `pread` into the slot on a descriptor opened with `F_NOCACHE`. `EVFILT_AIO` is an option the lane
  measures against them.

A thread for the loader, rather than a ring per worker as vmcache has, keeps the pool's IO one
sans-IO core that `Sim` drives (section 7). It also keeps workers out of the kernel, since only
IO driver threads block there (rule `deadline-waits`). The hand-off costs about 2 \u00B5s against a
50 to 100 \u00B5s read. Moving submission to the workers later is an open question.

**The effect.** `Effect::Read` returns a `Vec<u8>`, which would mean one more copy and one more
allocation per miss. The pool needs reads into memory it owns, so blaze-core gains a pair of
effects:

```rust
/// `into.len` bytes at `at` of `file` read into the registered span `into` (fewer at the
/// end of the file); completes with `Done::ReadInto { len }`.
Effect::ReadInto { file: FileName, at: u64, into: Span, tag: Tag },
/// The registered span `from` written at `at` of `file`; durable once a later `Sync` completes.
Effect::WriteFrom { file: FileName, at: u64, from: Span, tag: Tag },
```

A `Span` is data, not a pointer: `{ region: SpanRegion, offset: u64, len: u32 }`. The pool registers
its region with the driver once (`blaze_core::Spans::register`), and the driver turns a span into an
address. So `Effect` stays `Clone + Eq + Debug`, a test can log it, and `Sim` can carry it out
against the same memory. The asker's contract is the one `Effect::Map` has, turned around: while a
`ReadInto` is in flight, nothing reads or gives back its span. The pool upholds that with the
`Loading` state.

### Verification on load

The store checks a value's bytes against its BLAKE3 content hash before handing them out, once per
record (`Look::read`). Today that is sound because a mapping's bytes never change. Bytes in the
pool are read again after every eviction, so a reload needs its own check, and hashing every record
again on every reload would cost as much as the read. The image already shows the answer: a CRC32C
per group, checked the first time a read reaches the group (the module comment of
`crates/blaze-image/src/lib.rs`).

The store does the same per **block**:

- Each pack gets a sidecar `checks-<n>` (`crates/blaze-cas/src/checks.rs`), one CRC32C per 4 KiB
  block of the pack (`disk::BLOCK`). A block never changes once committed, because every commit
  pads its packs to a block (#1298). So the sidecar is appended to, never rewritten. It costs 4 bytes
  per 4 KiB.
- A load checks every block of the pages it read before `Loading -> Loaded`. That is 16 CRCs per
  64 KiB page, about 3 \u00B5s with the CPU's CRC instructions (`blaze_ids::crc32c`).
- A block that fails makes the page `Corrupt`, and every record in it is refused as `Corrupt`,
  naming the pack, the block and the record. It is never served. The page stays refused until
  `Store::verify` or a settle rewrites the record elsewhere.
- BLAKE3 stays once per record per process, under its `Flag::Verified` bit (`crates/blaze-cas/src/flags.rs`). A reload of bytes that matched
  their hash before, and match their block CRCs now, is the same bytes for any change that is not
  deliberate. Deliberate changes are kept out by the store directory being the user's alone
  (`Effect::Private`). That is enough (decided below): a reload does not hash again.

The sidecar changes the store's format. `FORMAT` goes from 3 to 4, and a store of format 3 refuses
to open, naming both, as every change of format does (docs/v2-store.md, "Opening"). There is no
migration. The store is rebuilt from what made it.

### Errors

Every failure is typed and belongs to the one task that read. None of them is a signal.

| what failed | what the reader gets | what else happens |
|---|---|---|
| the read (EIO, a descriptor gone) | `ReadError::Read { key, file, at, len, why }` | the page returns to `Unloaded` and the next reader tries again; the store counts failures, and `FAILURES_MOST` in a row break it, as for its writes |
| a short read inside the committed length (the file was cut by something other than blaze) | `Corrupt`, naming the file and both lengths | the store notes it in its problems |
| a block CRC | `Corrupt`, naming the pack, the block and the record | the page is `Corrupt` |
| the content hash, on a record's first read | `Corrupt` | as today |
| a run that cannot be loaded because pinned bytes fill the pool and nothing is evictable | `ReadError::Full { key, pinned, ceiling }`, once its bounded wait for room ends | the holders are named from the pin counters (section 4) |

A query that reads a value and gets an error fails with it, naming its key (rule
`errors-name-the-thing`). It never computes the value again in silence (rule `no-fallback`). That
is the rule `Corrupt` groups of the image already follow.

## 3. Writes

Records are immutable and content-addressed. The write path stays the group commit of #1298
(docs/v2-store.md, "A group commit"). What changes is where the bytes sit.

1. **Put.** A task encodes its record into its writer's segment (`Writer`, `segment.rs`) and
   publishes a spot, as today. It is readable at once from the segment (`Place::Segment`). The
   task writes its output once, into the segment, never into a heap tree kept until something
   serializes it.
2. **Drain.** `Store::flush` drains each segment's records straight into their pack's window in
   the pool, at the offsets they will have on disk. This replaces `Pack::memory`. Before writing
   into a pack's last page, the commit pins it. The page may hold records already committed and
   being read: the commit writes only bytes past the committed length, which no reader can reach,
   so readers and the commit never touch the same bytes. If that last page was evicted, the commit
   loads it first, one `ReadInto` of at most 64 KiB, because the page's words will say `Loaded`
   for the whole of it afterwards. The evictor passes over the last page of a pack written in the
   last two commits, so this is rare.
3. **Write.** Each pack written to is cut back to its committed length (`Truncate`). Then its new
   bytes are written from the pool with `WriteFrom`, block-aligned and `O_DIRECT`. The pages are
   the write buffer, so there is no copy into a `Vec` for the effect. Its new block CRCs are
   appended to `checks-<n>`. Each file is then synced (`Sync`), asked for only once its write
   has completed. A new index table, when one is due, is written the same way, into the inactive
   slot's window.
4. **Flip.** Once all of that is durable, the head is written (`Effect::Write`: a temporary file,
   sync, rename, directory sync). It names the new committed lengths. The checks file's length is
   the pack's committed length divided by 4096, so the head needs no new field for it.
5. **Publish.** After the flip, each spot is retargeted to `Place::Pack` with one swap, as today.
   The segments' chunks are retired at task boundaries, and the commit lets its pins go. The pages
   stay `Loaded`: what was just made is the likeliest thing to be read next. The link reads the
   functions codegen just put. No remap happens, so the 'after the flip the store maps each grown pack
   again' step of today goes.

**What a crash leaves.** The pool is process memory, so a crash loses it and nothing in it was ever
the only copy. On disk, everything the head names is whole: each pack up to its committed length,
and its checks to the same block. Past that there may be a torn append, in the pack or in its checks
file, which nothing reads and the next commit cuts. The inactive index slot may be torn, and the head
does not name it. The head itself is the old one or the new one, never part of either.

**Recovery: finding the last good record.** The open reads `head` (`ReadAll`). Then for each pack
it reads, through the pool, the pages from the pack's indexed length to its committed length. It
checks each block against the checks file and each record header by its CRC and magic, and builds
the delta from those headers, as it does from the mapping today. The last good record of a pack is
the last header before the committed length. A header that fails inside the committed range, a
failing block CRC there, or anything but zeros where a commit's padding is, stops the store, naming
the file and the block, as today. Bytes past the committed length are never read. The pages the scan
loaded stay `Loaded`: they are the records the last build wrote, and the next build reads them
first. A daemon that crashes in the middle of a read leaves nothing at all, since a read writes
nothing on disk.

The writer's durability is the order of the effects it emits (rule `sans-io-cores`), so `Sim` can
tear and crash it at any point. That proof is section 7.

## 4. Eviction

### The one evictor, and what it ranks

[memory-pool.md](../../../memory-pool.md#3-every-cache-is-a-hot-tier-over-the-store) puts one
evictor over every cache and ranks entries by the least cost to rebuild per byte, measured. The pool
is the first cache on it. It implements the evictor's interface (`name`, `charge`, `cost`,
`evict(bytes) -> freed`, memory-pool.md's order step 3) as `pool::Sweep`
(`crates/blaze-cas/src/pool/evict.rs`, `#![blaze::sans_io]`).

- **Across caches.** A pool page costs one read to rebuild, measured as the loader's
  latency per byte from its `pool load` spans. That is far less than recomputing a memo or a
  function, so the evictor takes pool pages before anything it would have to recompute. That is the
  point of the pool: the values in it can always be had again, cheaply.
- **Within the pool.** It uses CLOCK. The sweep walks the windows' words from where it stopped. A
  page with `referenced` set gets it cleared and is passed over. A page that is `Loaded`, unpinned
  and unreferenced is a candidate.
- **Index pages and recent pack tails.** Index pages count four times their bytes, because one index
  miss stalls every lookup it covers. The last page of a pack written in the last two commits is
  passed over.
- **Prefetched pages** that no demand read has pinned enter with `referenced` clear, so they are
  the first to go (section 5).
- **Bounded.** The sweep visits at most twice the pool's pages per call and ends in a typed outcome:
  the bytes freed, and whether it stopped short because everything else is pinned
  (rule `bounded-progress`).

### Giving pages back, and the TLB

A batch of up to 256 candidates is moved `Loaded -> Evicting`. Adjacent pages are merged into
ranges, and the batch is given back in as few calls as the system allows. The new
`Region::evict(&self, ranges: &[Range<usize>]) -> usize` does the calls, over `os::evict` in
`crates/blaze-mem/src/os.rs`:

- **Linux: `MADV_DONTNEED`.** The pages are freed now, the resident set and the cgroup charge
  fall at once, and the next touch reads zeros. The whole batch is one `process_madvise` of the
  daemon itself (`PIDFD_SELF_THREAD_GROUP`, Linux 6.14, so no descriptor is opened or inherited by a
  child) with one iovec per range. Linux 6.13 lets `process_madvise` take any advice
  for the caller's own process, and 6.16 flushes the TLB once per call for `MADV_DONTNEED` rather
  than once per range. blaze's floor is 7.2 ([memory-pool.md section 7](../../../memory-pool.md#7-what-the-kernel-does-for-it-linux-72-macos)). Lane 1 read both in
  `mm/madvise.c` (7.4-rc2): `process_madvise` skips its remote-only advice check when the target's
  mm is the caller's, and `vector_madvise` gathers the TLB once around every range for
  `MADV_DONTNEED`. It returns the bytes of the leading ranges it finished, so `Region::evict`
  gives back a prefix of its ranges. `os::check_evict` proves the call once per process and stops
  naming the kernel's release if it fails.
- **macOS: `MADV_FREE_REUSABLE`** per range, which takes the pages out of `phys_footprint` at
  once (`os::purge` does this already). A load's first step on macOS is `MADV_FREE_REUSE` over the
  run (`os::reuse`), so the footprint counts the pages again before they are written. A loader
  also zeroes what a short read left of the run, because reusable pages may hold anything.
- **Never plain `MADV_FREE`.** It is lazy: the kernel still counts the pages (in macOS's
  footprint, in Linux's RSS and cgroup charge) until it gets around to taking them. That is exactly
  the "freed but still counted" gap this work exists to close (rule `one-memory-measure`).

After the calls return, the batch moves `Evicting -> Unloaded`, and `Tag::Pool` drops by the bytes the
calls reported freed. A refused call leaves its pages resident and uncounted as freed, as
`os::purge` does today. It costs memory, not correctness.

**What the TLB costs.** On Linux, `MADV_DONTNEED` flushes the TLB on every core running the daemon,
with an IPI each. That is why vmcache batches its evictions, and why its authors built a kernel
module (exmap) for eviction rates near a million pages a second. blaze's rate is far lower. A build
that cycles 50 GB through the pool evicts about 800,000 pages over minutes, a few thousand a second,
and at 256 pages a batch that is tens of flushes a second. On Apple silicon, TLB invalidation is a
broadcast TLBI instruction, with no IPI. The `pool evict` span times each batch, and the flush's
share shows up there before anyone tunes it (rule `slow-is-p0-traced`).

**Not `MAP_DROPPABLE`.** [memory-pool.md section 7](../../../memory-pool.md#7-what-the-kernel-does-for-it-linux-72-macos)
maps caches droppable, so the kernel can drop their pages under pressure. That cannot work under
pinned, zero-copy reads: the kernel could zero a page while a `P::Ref` reads it. The pool keeps
eviction its own, and this supersedes that bullet for the store's tier. `memory.reclaim` of a
cache cgroup is moot for the same reason: the pool's pages are only ever given back by its own
evictor.

### With admission and the ceiling

The admission lane's core is `crates/blaze-graph/src/footprint.rs` (branch kept-memory-1109). The
pool joins it as follows:

- `PlaneInput::Footprint` gains `evictable: u64`: the pool's `Loaded` bytes, less those pinned and
  those the sweep passes over. `Charge::read` keeps it beside `kept`. The footprint still counts
  the pool's bytes, because they are memory the kernel enforces. The pool is not a separate
  account (rule `one-memory-measure`).
- `footprint::Step` gains `Evict { bytes }`, which `past_ceiling` tries after `Purge` and before
  `Wait`, `GiveWay` or `Fail`. A purge is free, since freed memory costs nothing to rebuild. An
  eviction costs reads later. Waiting costs the build. `TooBig` then means the task does not fit
  even with the pool emptied of everything unpinned, and its message says the pinned bytes and who
  holds them.
- The daemon's ceiling guard (`CeilingLook::Purge` on that branch) asks for an eviction after the purge
  and before it exits with `MEMORY_CEILING`.
- **The pool grows only into room.** It has no share and no fixed size. Before a load, the loads core
  checks the plane's last room (budget less charge, published by the plane as an atomic). It evicts
  first when the load would pass that room. A demand load that cannot make room waits, bounded and
  named, and ends in `ReadError::Full`. A prefetch is simply not issued (section 5).
- Pinned bytes are charged to the holding task's memory account (`arena::account`) as they are
  pinned, so its realized bytes and its peak include them, and the grades see them.

## 5. Prefetch

Prefetch is the pool's to plan, because only the pool knows which pages are in memory and which are
in flight, and only blaze knows which pages a build is about to read.

### Where MADV_WILLNEED falls short

With a mapping, the only prefetch is `madvise(MADV_WILLNEED)` (or `posix_fadvise`). It cannot do what a
build needs:

- **No completion.** It starts read-ahead and returns. Nothing says when the pages are there, so no
  task can be started after them, and nothing can wait on them without faulting.
- **It still faults.** It fills the page cache, not the process's page tables. Every page still
  takes a fault on first touch, behind the `mmap_lock`.
- **No priority.** The kernel's read-ahead competes with demand faults on equal terms, so a
  prefetch can delay the read a task is blocked on.
- **No budget, no accounting.** The kernel may drop the advice, may evict the pages before they are
  used, and says neither. Hit rate and wasted bytes cannot be measured, so nothing can be tuned. On
  Linux the pages it brings in are charged to the daemon's cgroup all the same.
- **No dedupe and no batch blaze sees.** It is one call per range. It cannot merge two views'
  ranges, and it cannot cancel a range nobody wants any more.
- **Best-effort on macOS.** There the advice is bounded by the unified buffer cache's cluster read,
  and file pages it brings in are not blaze's to evict.

In the pool, a prefetch is the same explicit read as a demand miss, into the same slots, through the
same state word. That gives everything the list lacks:

- Dedupe: a page already `Loading` or `Loaded` is not asked again.
- Completion: an event, which a task can wait on.
- Priority: two queues in the loads core.
- A budget in bytes and in reads in flight.
- Cancellation: an asked prefetch that is not yet submitted is dropped when the budget shrinks.
- A `prefetched` bit per page, which the first demand pin clears and counts as a hit.

### What to prefetch, from small metadata

The planner, `pool::prefetch::Plan` (`crates/blaze-cas/src/pool/prefetch.rs`, `#![blaze::sans_io]`),
never reads a record to decide. It works from three sources of metadata that are already in memory,
each producing page runs `(pack, first page, pages)` with a probability:

1. **The query graph.** The dense engine has no reverse edges. Each node row has instead a 128-bit
   input summary, a Bloom filter of the input fields it read, transitively (`Summary`,
   `input_bits`, `NodeRow` in `crates/blaze-query/src/db/storage.rs`).
   - A change set's fields set bits. The memos whose summaries share a bit with them are the ones
     the next walk verifies, and maybe reruns. False positives cost a page read and nothing else.
   - The scan is over one column of 32-byte rows. It is split across the pool's workers, and bounded
     by fuel, so a huge database plans part of its cone, not a stalled start.
   - For each such memo, its value's place (once memos are on the pool, section 6: the value row
     holds its `Loc`) gives a run. Its value is read for early cutoff by `==` when it reruns, and
     by its dependents when they do.
   - Probability: 1 for a memo whose summary matches a changed field exactly, lower for one matched
     only through the transitive bits.
2. **Learned from past builds.** For each input key that changed in a build, a record of the page
   runs that the build read after the change.
   - The record is a Space-Saving sketch of the top 256 runs, each with a count, plus the number of
     builds the input changed in.
   - Probability is count over builds, decayed by half every 64 changes, so a run the code no longer
     reads fades out.
   - It is built at each build's end from the `pool_reads` table (section 5, "Counted") by a
     sans-IO maintainer pass. It is kept as a blaze-history section of the daemon image
     (`crates/blaze-history`, beside admission's estimates).
   - This is what covers what the graph cannot see: the link reading every unit's code after one
     function changes, a test program's objects, build-script outputs.
3. **The per-view warm set.** At a build's end, each view saves the pages its build pinned, as a
   sparse bitmap of runs (`pool::WarmSet`, run-length encoded by pack).
   - It is saved as a small store record keyed by the view (`Key::of(WARM_SCHEMA, producer, [view])`),
     made durable by the next commit, so it survives a daemon crash.
   - At the view's next start (a daemon restart, or the first build of a view after one) it is
     replayed as a prefetch of probability 1/2.
   - This is InnoDB's buffer pool dump and load, and `pg_prewarm`'s autoprewarm.
   - It is what makes a restarted daemon's first check fast. It also prefetches the daemon image
     once the image is on the pool (section 6).

Page runs name a pack by its number in the head, and packs are only appended to. So a run stays
valid until its pack is retired, and the planner drops runs of retired packs. A settle that moves a
record makes the old run dead weight. The waste counter shows how much, and the learned sketch
forgets it.

### One batch for a change set, across views

When a check or build starts for a change set, which may cover several views (worktrees) at once,
the daemon calls `Plan::for_change(changes, views)` before it schedules the first task. It takes
the union of the three sources over every view, drops pages already `Loading` or `Loaded`, and
ranks what is left by probability. Then it cuts the list at the budget (below), sorts by file and
offset, and merges adjacent runs into reads of up to 1 MiB.

Pages are content-addressed records in one store. Two worktrees with the same dependencies read the
same std, the same crates' memos and the same compiled functions, so the same pages, and the union
holds each page once.

The batch goes to the loads core as one ask. That is one `io_uring_enter` submitting every read on
Linux, and one hand-off of every read to the file threads with one trigger on macOS. Planning is
bounded: it has fuel for 2 ms and ends naming how much of the cone it covered (rule
`bounded-progress`). Its span `prefetch plan` carries the pages and bytes asked from each source.

### Never crowding out demand

- **Two queues.** The loads core keeps demand and prefetch apart. It submits prefetch reads only
  when no demand read is waiting to be submitted, and a demand read always goes before a queued
  prefetch.
  - On Linux a prefetch read is tagged with `IOPRIO_CLASS_IDLE` in its `ioprio`, which the block
    layer honors under `mq-deadline` and BFQ.
  - On macOS the prefetch threads run with the throttled IO policy (`setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE)`).
- **In flight.** At most `PREFETCH_IN_FLIGHT = 64` prefetch reads at once, against the table's
  4096. A demand read never waits for a table slot that a prefetch holds: the slots are split, and
  prefetch has its 64 and no more.
- **A budget in bytes.** At most the smaller of an eighth of the ceiling and the room that eviction
  of unreferenced pages could give. A prefetch never evicts a page with `referenced` set, or a page
  pinned in this build, to make its room.
- **Caught up.** A demand read for a page a prefetch is `Loading` waits on that load. It is counted
  as a late hit, and the read is not issued twice.
- **Dropped first.** Prefetched pages that no demand read has pinned are the first the evictor takes.

### Counted

Each build writes a `pool_reads` row per load (the profile's tables, docs/profiling.md):

- build, view, pack, first page and pages;
- the class: demand, or prefetch from graph, learned or warm;
- latency, and the task that waited, if any;
- the outcome: a hit (the first demand pin while `prefetched` was set), late, wasted (evicted or
  never pinned before the build ended), or a demand miss no source predicted.

Counters per build: `prefetch asked bytes`, `prefetch hit bytes`, `prefetch wasted bytes` and
`demand missed bytes`.

`blaze explain prefetch [build]` ranks the sources by hit rate and wasted bytes. Thresholds,
weights and budgets are then set from those numbers, and every change to them quotes before and
after (rule `slow-is-p0-traced`).

## 6. What moves onto it, in order

1. **The store's own reads.** Packs, index slots and files of their own are read through the pool,
   and the commit writes from it (lane 4). No caller changes, since `Look` and `Pinned` keep their
   interface. This is what makes the rest worth doing.
2. **Codegen outputs.**
   - `CompiledFunction` (`crates/blaze-codegen/src/lib.rs`) is a serde struct of `Vec`s, kept in
     `driver::Built` until the link. It becomes a blaze-format record with a `Persist` impl. The impl
     is hand-written, naming the generic `for const` impl's issue, until that lands. Its `Ref` is
     `CompiledRef<'a>`, giving code, relocs, data, unwind and lines as `Slice`s.
   - The unit's task writes each one once into its writer's segment, as `Kind::Code`, under
     `fncache.rs`'s key (rule `cache-by-code-id`). `Built` holds keys.
   - The linker (`crates/blaze-link`) pins what it reads.
   - Object code stops being a `Vec<u8>` kept until the link (memory-budget.md section 4). Each
     unit's objects are put as they are made, and the link reads them pinned.
   - The 13.67 GiB codegen heap peak is the number this step should take down.
3. **Query memos.**
   - A memo's value row (`ValueRow` in `crates/blaze-query/src/memo.rs`) names a store record's
     place in the pool, instead of bytes in a segment or a mapping. The task that ran the query
     writes the value once into the store's segment, as `Kind::Query`.
   - A hit pins through the task's `PinSet`. The hot row's inline copy and its seqlock stay, for the
     values small enough to sit in it.
   - `Stored::Disk` becomes the only place a value has, beside the inline copy and the segment
     before its commit. `Database::evict` and `Squeeze::caps`' value sweep become the evictor's work
     on the pool.
   - The cold files of `Pending::ColdWrite` and `Pending::ColdMap` go.
   - The 31.5 GiB of query storage on a plain `blaze check` of blaze is the number this step should
     take down: from bytes that only grow to an evictable set.
4. **The daemon image.**
   - It is mapped today, so that a start reads only the spine. It moves onto the pool last, because
     its restart-to-first-answer budget (rule `persisted-state-round-trips`) is the thing to protect.
   - The image file gets a window like any store file. A group (64 KiB by default) is a page, so
     the group CRC that `Image::group` checks on a first read becomes the page's load check, and
     `Image::group` and `Image::range` pin.
   - A cold start then reads the spine with one explicit read, and the view's warm set prefetches the
     rest as one batch. That should beat a mapping's fault per page, and the lane measures it against
     the mapping.
   - Until it moves, the image may stay mapped. It is read once per daemon life and its values are
     immutable. This is the one mapping, besides `Kind::Machine`'s, that the hot tier keeps for a while.

## 7. Proof

### The DST

`crates/blaze-cas/src/pool/tests.rs` is one Hegel test (the `testing` skill) of the real pool, loads
core, sweep and store, driven by `blaze_core::sim::Sim`. `Sim` gains `ReadInto` and `WriteFrom`
against the registered region. The test's `os::evict` poisons the bytes it gives back with
`0xDB`, standing in for Linux's zeros and macOS's anything.

What it draws:

- **Operations:** puts of records of drawn sizes, from 16 bytes up to several pages; flushes;
  pinned reads, held across later steps and released; optimistic reads; `PinSet` reads with a
  task end or a frame reset; eviction batches; prefetch batches; settles and pack retirements;
  crashes; reopens.
- **Actors and schedule:** several readers, the commit, the sweep and the loader's completions.
  Which one takes its next step is drawn, at the granularity of state-word operations.
  `Sim` is one thread, so the test steps each actor's state machine one CAS at a time. Two
  threads' atomics interleaving at every operation waits for `hegel::sim`'s scheduled atomics,
  and the test moves there once they are on the fork's main.
- **Faults, as a swarm:** failed reads, short reads, torn writes, failed and held syncs, ENOSPC
  (`fill`), a block of a pack changed on disk (rot), and completions delayed and shuffled.
- **Crash points:** after any completion count. The reopen runs the open's recovery scan.

Its oracle, checked at every step and after every crash, against a `BTreeMap` model:

1. Every pinned or validated-optimistic read returns exactly the bytes put under its key, or a
   typed error the faults explain. Never poison, zeros or torn bytes.
2. No page is given back while it is pinned or `Loading`.
3. After any crash and reopen, every key whose put a completed flush covered reads its bytes. A key
   put after it reads its bytes or is absent. The store always opens, or stops naming a fault the
   draw put on disk (rot inside the committed range).
4. `Tag::Pool` equals the `Loaded` and `Loading` pages times `PAGE` at every step. Pinned bytes
   equal the sum of the guards alive.
5. Every ask ends, loaded or failed, within a bound of steps. Every waiter wakes.
6. The prefetch budget in bytes and in flight is never passed. A demand read is never queued
   behind a prefetch. `asked = hit + late + wasted + resident` for prefetched pages.

Planted bugs, each a test-only switch (`pool::Plant`) that the oracle must catch within its case
budget:

1. Eviction does not raise the version, so a stale optimistic copy validates.
2. Eviction ignores `pins`.
3. `Loaded -> Unloaded` before the `madvise` (no `Evicting`), so a reload's bytes are dropped.
4. `Loading -> Loaded` published before the read completes.
5. The block checks are skipped on load, so a torn or rotted block is served.
6. A failed read leaves the page `Loaded`.
7. The head is written before the packs' and checks' syncs complete (as the store's crash tests plant
   today).
8. The commit writes into a pack's last page without pinning it, so the sweep evicts it in the
   middle of the write.
9. A prefetch is submitted ahead of a waiting demand read.

The footprint DST of the admission lane (`crates/blaze-graph/src/footprint/tests.rs`) gains a pool:
a simulated evictable tier with drawn pinned shares. Its oracle gains "`Wait`, `GiveWay` and `Fail`
only after an `Evict` that could not make room", with planted bugs for a gate that skips the pool
and an eviction that counts pinned bytes as freed.

Round-trip: a store written, crashed and reopened reads every value byte for byte equal to a fresh
build's puts (rule `persisted-state-round-trips`), on both the old and the new format codecs
during lane 4.

### The measurement

A self-check on the Mac, run as memory-footprint.md runs it: a tree-built blaze running
`blaze check` of blaze's own tree, with `BLAZE_DAEMON_MEMORY_GB=48`. Then the same with
`BLAZE_DAEMON_MEMORY_GB=24`, to make the pool evict. The run names the tree's digest and the
binary's hash (rule `measure-the-exact-tree`). Each of the trees before and after lanes 4, 7 and 9
is measured. From the counters (`footprint bytes`, `live bytes`, `frame bytes`, and the new
`pool loaded bytes`, `pool pinned bytes` and `pool evicted bytes`), the run must show:

- **Footprint tracks live memory.** At every sample, the footprint less the sum of live heap,
  frames, the pool's loaded bytes and blaze-mem's regions is under a quarter of the footprint
  (memory-footprint.md's acceptance). It shrinks lane by lane as outputs leave the heap.
- **Eviction acts at once.** Each `pool evict` span's bytes show up as a drop in the footprint by
  the next sample (100 ms), within a page per batch.
- **The ceiling holds.** At 24 GiB the build finishes with no exit 77, the footprint under the
  ceiling, and the pool's evicted bytes being what bought it.
- **The cause, removed.** The run is repeated with the sweep turned off (a debug setting the lane
  adds). The footprint must then pass the ceiling, or the run must fail with `ReadError::Full` or
  `TooBig`. Removing the cause and watching the effect go is rule `test-the-cause`.
- **Speed.** Demand miss latency p50 and p99 from the `pool load` spans. The check's wall time
  against the mapped tree, from the spans. The prefetch hit rate and wasted bytes on the edit
  harness (docs/harness.md) and on a second worktree's first check.

`blaze explain memory` (memory-footprint.md step 1) gets a pool row. After the last lane, this
check runs as a step of the self-build in CI.

## 8. The lanes

Each lane is one pull request or a few, landed through `.github/scripts/land.py`, with its DST where
it touches concurrent or durable code. Lanes 1 and 2 can run side by side. The rest are in order.

1. **blaze-mem: giving back a sparse range.**
   - `Region::evict(&self, ranges)` for a sparse region, over `os::evict`: Linux
     `process_madvise`(`MADV_DONTNEED`) of its own pidfd; macOS `MADV_FREE_REUSABLE`; Windows
     `MEM_DECOMMIT`.
   - `Region::reuse` for sparse regions; `blaze_mem::racy::copy_out`; `Tag::Pool`.
   - A test of the 4 TiB reservation on both systems, and a check that the kernel takes
     `MADV_DONTNEED` through `process_madvise` for its own process, failing by name if not.
   - A property test that the tag's bytes follow evictions and reuses.
2. **blaze-core: reads into memory the asker owns.**
   - `Effect::ReadInto`, `Effect::WriteFrom`, `Span`, `Spans::register` and `Done::ReadInto`.
   - Files opened `O_DIRECT` or `F_NOCACHE` when the asker says so, and `ioprio` on Linux.
   - In `uring.rs`, `kqueue.rs`, the reference `threads.rs`, `iocp.rs` and `sim.rs`. `Sim` gains
     short reads, failures, and torn `WriteFrom` on a crash.
   - The drivers' differential tests (`crates/blaze-core/src/tests.rs`) cover the new effects.
     `sched::host::Event::rearm` lands here too.
   - Built on branch pool-effects-1109 (`crates/blaze-core/src/spans.rs`). The asker says how
     with two fields: `cache: Cache` (`Kept` or `Bypassed`; bypassing takes an offset, address
     and length of whole `DIRECT_ALIGN` = 4 KiB blocks on every driver, `Sim` too, and a file
     system that cannot bypass fails, naming the file) and, on a read, `priority: Priority`
     (`Demand` or `Idle`; Windows refuses `Idle`, having no IO class per read). A
     `WriteFrom` completes with `Done::Written`. A read's span past what it read holds
     anything (a read around the cache writes whole blocks), so the loader zeroes it, as
     section 4 says. io_uring keeps a positional descriptor per file (never `O_APPEND`, which
     on Linux turns a positional write into an append) and reads into the span's address;
     kqueue's and IOCP's file threads `pread` and `pwrite` through `threads::run`, opening per
     effect for now. A ring that fails with one in flight fails it with `ABANDONED_SPAN`: the
     kernel may still write the span, so the pool never uses that page again. `Sim` scribbles a
     read's span as it is submitted, takes a write's bytes as it completes, lands any of a failed
     or in-flight write's 512-byte sectors, tears unsynced writes from memory by sector at a
     crash, and has `hold_reads`, `fail_reads`, `short_reads` and `rot`. `Memory` is a
     registered buffer for tests. `crates/blaze-core/src/sim/pages.rs` is a Hegel DST of a
     pool-shaped page store on them, with planted bugs 4 to 8 of section 7 in miniature: the
     scaffolding lane 3's DST grows from.
3. **blaze-cas: the pool core and its DST.**
   - `pool.rs` (`Pool`, `Word`, `Windows`, `Run`, `PinSet`, pins and optimistic reads).
   - `pool/loads.rs`, `pool/evict.rs` and the driver `pool/drive.rs`.
   - The DST of section 7 with planted bugs 1 to 6, over files the test writes. The store does not
     use it yet.
4. **The store on the pool.**
   - Packs, index slots and files of their own read through the pool, with `checks-<n>` and
     `FORMAT` 4. The open's recovery scan as reads. The commit draining into the pool and writing
     with `WriteFrom`. `Pinned` as pins.
   - The DST grows planted bugs 7 and 8. The store's existing crash and model-check tests
     (`crates/blaze-cas/src/tests.rs`, `tests/model_check.rs`) run on the pool.
   - Deletes: `Pack::mapping`, `Pack::memory`, `PackView::mapping`, `Index::mapping`,
     `ExternalView::mapping` and `External::mapping` (except for `Kind::Machine`),
     `PinnedBytes::Mapped`, the remap after every commit, and the store's every `Effect::Map` but
     `Kind::Machine`'s.
   - docs/v2-store.md is brought up to what it then does.
5. **Eviction with admission.**
   - The pool as the first cache on the one evictor. `PlaneInput::Footprint::evictable` and
     `footprint::Step::Evict`. The ceiling guard evicts before it exits. Pinned bytes go into the
     task's account.
   - Telemetry: the `pool loaded bytes`, `pool pinned bytes` and `pool evicted bytes` counters;
     the `pool load` and `pool evict` spans; the `pool_reads` table; and the pool row of
     `blaze explain memory`.
   - The footprint DST gains the pool.
   - The first measurement, of the store's own reads.
6. **Prefetch, part one: priority, budget, counting, warm sets.**
   - The loads core's two queues, the budget, the `prefetched` bit and its counting.
   - `pool::WarmSet` saved at each build's end and replayed at a view's start.
   - `Plan::for_change` with the warm source alone, deduplicated across views and merged into one
     batch. `blaze explain prefetch`.
   - The DST grows planted bug 9.
7. **Codegen outputs on the pool.**
   - `CompiledFunction` as a record, written once.
   - `driver::Built` of keys. The linker pins. Objects are put as they are made.
   - Deletes: the `Vec<CompiledFunction>` kept in `Built`, and `write_object`'s whole-unit buffers kept
     until the link.
   - Measured.
8. **Prefetch, part two: the graph and the learned sketch.**
   - The summary scan as a source.
   - The per-input Space-Saving sketch, built from `pool_reads` at each build's end and kept in a
     blaze-history section.
   - Thresholds set from lane 6's numbers.
9. **Query memos on the pool.**
   - `ValueRow` naming a store record. Values written once by their query's task. `PinSet` hits.
   - Deletes: the cold files (`Pending::ColdWrite`, `Pending::ColdMap`), the memo store's value
     segments for committed values, `Database::evict` as a separate path, and `Squeeze::caps`' value sweep.
   - Measured on the self-check.
10. **The daemon image on the pool.**
    - The image's window, groups as pages, and its cold start through the pool and the warm set.
    - The restore budget measured against the mapping.
    - Deletes: the image's `Effect::Map` and its mapping in `crates/blaze-cargo/src/serve/image.rs`.
11. **The check in CI.** Section 7's measurement as a step of the self-build.

After lane 10 the only file the store's tier maps is a `Kind::Machine` module, through its loader.
`fs/mapped.rs` stays for what is not the store: the executables and libraries the splice and the link
read, and the proof's outputs.

Superseded by this design:

- memory-budget.md section 3's "read back by map when needed (file-backed pages the kernel can take
  back)": the pool's own reads replace it.
- memory-pool.md section 7's droppable arenas and `memory.reclaim` for store-backed caches (section 4).
- memory-pool.md section 3's "link inputs ... (mmapped)": link inputs move onto the pool in lane 7.

## Decided

Andrew's answers, 2026-11-09:

- **Re-verification.** A CRC32C per 4 KiB block on every load is enough; BLAKE3 stays once per
  record per process (section 2).
- **The format bump.** Format 4 refuses stores of format 3 with no migration; the store is
  rebuilt.
- **The macOS reservation.** blaze already reserves thousands of GiB on macOS (its memory line
  showed 4913 GiB reserved), so the 4 TiB reservation is assumed to be granted; lane 1 keeps the
  reservation test.
- **The image.** The daemon image stays mapped for now, the one exception besides compiled wasm
  modules (`Kind::Machine`). Moving it onto the pool (lane 10) comes after, measured.

## Open questions

1. **One page size.** Is 64 KiB right, or should files of their own (large objects) get 2 MiB pages?
   Lane 5's measurement decides.
2. **The loader thread, or rings per worker.** Start with the loader thread, and move submission onto
   the workers only if the hand-off shows in the spans?
