---
status: plan
summary: The long-term memory design. One budget per machine, run like a database buffer pool. Every byte is charged to a task's frame or to a named cache. Admission uses measured peaks by kind and input size. Every cache is an evictable hot tier over the content-addressed store, with one evictor. Nothing is buffered without a bound. One daemon per machine, and the kernel's limit as the backstop.
date: 2026-11-05
---
# Memory as one buffer pool

[memory-budget.md](memory-budget.md) admits work against a budget, and its section 7 adds the
ceiling that keeps blaze from taking a machine down. [v2/plan/design/memory.md](v2/plan/design/memory.md)
allocates by lifetime. This note is where both lead. The daemon's memory is one pool with one
budget, run the way a database runs its buffer pool. Every byte in it has an owner, every
owner is either a running task or a cache that can be rebuilt, and one evictor keeps the sum
under the budget.

## Why: what #1726 found

A cold `blaze test --workspace` on a 251 GiB VM, with an 81.21 GiB limit:

- **Admission did not hold.** The daemon reached 117.66 GiB because every cold unit phase was
  granted 64 MiB, the default for a kind never seen. Peaks were up to 3.04 GiB. This is fixed
  in admission (`Plane::sized`), but a prediction is still only a prediction.
- **Memory stayed held after the build.** Once the build ended the daemon sat at 85.6 GiB for an
  hour of test runs:
  - global heap: 32.8 GiB live, 97.9 GiB committed, up to 16.6 GiB of it freed but resident;
  - regions: 37.1 GiB, of which the query memo is 15.9, frames 5.8 and link 4.8;
  - heaps: codegen 4.6, intern 6.4 and store 5.7.

  None of it was charged to anything that could give it back.
- **Bookkeeping spiked at the build's end.** The plane heap went from 1.7 to 15.2 GiB and the
  profile heap from 0 to 15.0 GiB as the misses and 12.7M `key_parts` rows were handed to the
  history, buffered whole.
- **No daemon knew of the others.** Five daemons on a 128 GiB Mac summed to about 306 GiB and the
  kernel panicked.

## The design

### 1. Every allocation has an owner

An owner is either a **task's frame**, freed whole when the task ends
([v2/plan/design/memory.md](v2/plan/design/memory.md)), or a **named cache**: a `Pool` entry
with a name, a byte count and a cost to rebuild. Today's `blaze_mem::Tag`s count by subsystem,
which says where the bytes are but not who may give them back. A cache is finer than a tag. A
tag is the sum of its caches and its tasks' frames.

What is allocated with no owner is a bug the counters show. `Snapshot::heap_outside` is held
to a small fixed bound, and the profile's `heap_by_owner` table, empty today, is filled at
every build's end from the pool's owners. A test fails a build whose unowned heap passes the
bound. Long-lived structures that are neither tasks nor caches are each named: the interner,
the plane's graph, the store's tables. Each such fixed owner has a budget of its own, and
together they count against the whole.

### 2. Admission by measured peaks

A task's prediction is its kind's measured bytes per input byte times its input's size. The
history keeps it (`blaze_history`, by task kind and by package), so a new daemon's first build
already knows its kinds. Until a kind is measured the guess is its size times a per-kind
default ([memory-budget.md](memory-budget.md) section 2), never one fixed number. A task is
admitted when its prediction fits in what the evictor can make free: the budget, less the
running tasks' grants, less the caches that can't be evicted (pinned by a running task). A
task that grows past its grant does not grow the pool. Its frame's growth is charged against
the free space, so the evictor sheds caches first, and only then is admission held.

### 3. Every cache is a hot tier over the store

Each in-memory cache holds values that are already in, or can be put in, the content-addressed
store (`blaze-cas`). The cache keeps the hot copy and the store keeps the durable one:

| cache | what it holds | reloaded from |
| --- | --- | --- |
| query memo (`blaze-query`) | query results | recomputed, or the store's values by key (MIR, item trees) |
| codegen output | compiled functions, dev templates | the store's `Code` values |
| link inputs | objects and their symbol tables | the store's placed objects (mmapped) |
| front ends kept for a checkout | a checkout's whole database | a cold load (the most costly, evicted last) |
| plane heap | finished tasks' outputs not yet taken | the store (`Happened::Kept` puts by key) |

**One evictor** owns the budget. When free space falls under the line, or the system reports
pressure (memory-budget.md section 7), it evicts across every cache in order of the least
cost to rebuild per byte. That cost is measured: the time the value took to make, or to load
back from the store. Least recently used breaks ties. A cache is told to drop an entry and
reports the bytes it freed. The heaps then give their free pages back
(`blaze_mem::tag::give_back_heap`), so the process's footprint falls with the counts. Caches
never evict on their own schedule. salsa-style `lru = N` limits and per-subsystem fixed shares
are replaced by the one evictor.

### 4. Nothing is buffered without a bound

Every queue, buffer and table that grows with a build has a byte bound and streams past it:

- the profile history's rows (`key_parts`, `misses`, provenance) go to disk in batches of at
  most `facts::BATCH_TEXT_BYTES`, as they arrive, never held whole until the build's end;
- a build's misses are folded into the history as units end, not gathered in the plane;
- objects stream to their files as each codegen unit is done (memory-budget.md section 4).

### 5. One daemon per machine, one budget

[memory-budget.md](memory-budget.md) section 8. The pool's budget is the machine's, shared
through the ledger (section 7) by whatever daemons run.

### 6. The kernel's limit is the backstop

jetsam on macOS (`memorystatus_control`) and a cgroup's `memory.max` on Linux stay set at the
ceiling, so a bug in any of the above ends one daemon, with a typed error on the client, and
never the machine.

### 7. What the kernel does for it (Linux 7.2, macOS)

For the store's hot tier, [v2/plan/design/store-pool.md](v2/plan/design/store-pool.md#giving-pages-back-and-the-tlb)
supersedes the droppable arenas and `memory.reclaim` below: pinned zero-copy reads cannot let the
kernel drop a page, so the pool's own evictor gives its pages back.

blaze's Linux floor is 7.2, the ix VM kernel (docs/kernel.md). The pool uses the kernel
features below directly, with no probe and no path for an older kernel:

- **`MAP_DROPPABLE` (6.11) for evictable cache pages.** A cache's arena is mapped droppable, so
  under pressure the kernel discards its pages without asking the evictor, and with swap off
  there is no writeback. A dropped page reads back as zeros, so each entry carries a nonzero
  header word that the cache checks on every read. A zeroed header is a miss, and the entry is
  reloaded from the store, as an evicted one would be (section 3). The evictor's count of a
  droppable arena is its mapped size less what `mincore` says is gone. Only values the store
  holds go in a droppable arena: a value that has not been placed yet is not droppable.
- **cgroup `memory.reclaim` (5.19) for the evictor's own sweep.** The caches' arenas are
  charged to a child cgroup of the daemon's (`cache`) with the memory controller on. When the
  evictor needs `n` bytes it writes `n` to `cache/memory.reclaim` first. The kernel then takes
  the coldest pages (droppable and lazyfree heap pages), with nothing for blaze to rank. It
  falls back to its own cost order only for what the kernel gives back short (`EAGAIN`). The
  write blocks while the kernel reclaims, so it runs on the evictor's thread, never on the
  loop.
- **Per-cgroup PSI triggers.** Pressure is read from the daemon's own cgroup's
  `memory.pressure` (memory-budget.md section 7, "Pressure"; built in the ceiling PR). The pool
  arms a second trigger on `cache/memory.pressure`, so stalls on cache pages alone tell the
  evictor to slow its refills.
- **`MADV_COLLAPSE` (6.1) and mTHP for the big arenas.** The query and frame arenas, which live
  for a build, are `MADV_HUGEPAGE`. Once filled, they are collapsed in place with
  `MADV_COLLAPSE` on the evictor's thread, with mTHP sizes (`hugepages-64kB` on arm64) enabled
  for the daemon's anon memory. This cuts TLB misses and page-table memory (#1726's 97.9 GiB
  committed heap is about 200 MiB of page tables at 4 KiB). A collapse that fails (no
  contiguous memory) leaves the arena as it was, which is correct, and its span names it.
- **`memory.events` for the hard ceiling.** The 100 ms footprint watch stays the bound on
  macOS. On Linux the cgroup's `memory.high` (15/16 of the ceiling) already makes the kernel
  reclaim past that line, and `memory.max` is the kill. The watch is kept as the line in the
  log that names the daemon's tags before a kill, and an inotify watch on `memory.events`
  (`high` and `max` counts) wakes it as soon as the kernel acts, not on the next tick.

On macOS, the equivalents are the dispatch memory pressure source (built), `madvise(MADV_FREE_REUSABLE)`
for evicted arenas, and the jetsam limit where the entitlement allows it (memory-budget.md
section 7).

## Proof

A Hegel deterministic simulation test of admission and eviction together (the `testing`
skill):

- **What it draws:** tasks with random kinds, input sizes and real peaks (some above their
  prediction); caches with random entry sizes and rebuild costs; pressure events; crashes of
  tasks.
- **The oracle:**
  - the pool's charged bytes never pass the budget once eviction has run (no step admits a task
    that does not fit after every evictable byte is gone);
  - every task that fits alone is admitted within a bounded number of steps (progress);
  - an evicted value read again is reloaded equal to what was evicted.
- **Planted-bug controls it must catch:**
  - an evictor that skips a cache;
  - admission that counts grants but not caches;
  - a cache that reports bytes freed without freeing them;
  - an evictor that evicts a value a running task pins.

## The order it lands in

Each piece is its own pull request, admin-merged once `blaze check` and its focused test pass:

1. The ceiling and the machine ledger (memory-budget.md section 7).
2. Bounded buffering: key_parts kept per miss (done), then streamed by the history as units
   end; misses folded as units end.
3. `Pool` and owners. The pool API is `name`, `charge`, `cost` and `evict(bytes) -> freed`. The
   query memo, codegen output and front ends kept move onto it first, then fill
   `heap_by_owner`, then put the bound on unowned heap.
4. The one evictor, replacing `Squeeze`'s fair shares and the query `lru` limits. The DST lands
   with it.
5. Admission against evictable free space (section 2), with predictions kept in the history by
   kind and input size.
6. Link inputs and the plane heap as store-backed tiers.
7. One daemon per machine (memory-budget.md section 8).
8. The kernel's part (section 7): droppable cache arenas and the `cache` cgroup with
   `memory.reclaim` land with the evictor (4); `MADV_COLLAPSE` and the `memory.events` wake
   are each a pull request of their own.
