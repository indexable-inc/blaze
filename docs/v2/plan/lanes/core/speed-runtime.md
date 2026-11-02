---
status: plan
summary: speed-runtime's pass over the daemon's allocator counting, bump arenas and memory cap (blaze-graph arena.rs) and the Linux and macOS watch and backstop code (blaze-core): the state of the art for each, how the code compared, what changed and how it compares now.
date: 2026-10-29
---
# speed-runtime: arena and the drivers' watchers

What `speed-runtime` changed for [principles.md](../../goals/principles.md)'s "Fast code,
line by line". The plane (`plane.rs`, `admission.rs`) is in
[speed-runtime-plane.md](speed-runtime-plane.md), the pool, pump and scheduler in
[speed-runtime-pool.md](speed-runtime-pool.md), and the VFS in
[../vfs/speed-runtime.md](../vfs/speed-runtime.md). Nothing here has been built or run yet.

## `Counting`, the daemon's global allocator (`arena.rs`)

**Benchmark.** The per-thread fast path of mimalloc and snmalloc: a thread-local heap, a free
list pop, about a dozen instructions and no atomic. jemalloc's tcache is close. Statistics in
all three are per thread and merged rarely. The kernel's `percpu_counter` is the pattern for a
total many cores add to: a per-CPU delta, folded into the shared count only past a batch.

**Before.** Every allocation and free did a relaxed `fetch_add` on one of 64 shards (a locked
read-modify-write, about 20 cycles on Zen 5 when the line is local). The shards were handed out
round robin, so past 64 threads (a 192-core Turin runs 384) about six threads wrote each line,
and it bounced between cores. The thread-locals `NET`, `PEAK` and `SHARD` were three separate
TLS accesses, which on macOS are three `tlv_get_addr` calls. `held()` summed 64 lines that
other cores keep writing, so up to 64 cache misses, and it runs after every task's `Done`
(`exec.rs`, `blaze-cargo/src/serve/plane.rs`).

**Now.**
- One TLS block (`Local`) holds `net`, `peak`, `unflushed` and `batch`.
- An allocation does three adds and two well-predicted branches, and no atomic.
- A thread's `unflushed` reaches the one 128-byte-padded `HELD` only once it passes ±256 KiB,
  which is one relaxed add per 256 KiB of drift.
- At thread exit, a `const` thread-local with a destructor (`EXIT`) adds what is left. Once
  that has run, every later change is added at once, so no count is lost with a thread.
- The destructor is registered from the cold flush only. The allocation the registration
  itself makes comes back to the flush with `armed` set, and registers nothing.
- `held()` is one load plus the caller's own `unflushed`. It is exact for the calling thread,
  and within 256 KiB for each other thread (Linux's `percpu_counter` bound).
- `measure` is unchanged and exact, since it reads the thread's own `net`.

This matches the allocators' fast path in kind. What `Counting` adds on top of the allocator it
wraps is a handful of instructions on the thread's own cache line. Recommended wrapping:
`Counting<Jemalloc>` on Unix, as blaze-engine uses today (request to core).

## `Bump` (`arena.rs`)

**Benchmark.** bumpalo's downward bump ("always bump downwards"): one subtraction, one mask and
one compare per value, and a reset that keeps the last chunk.

**Before.**
- Upward bump with `checked_next_multiple_of`, which divides (20 to 40 cycles), on every value.
- A `RefCell` borrow and a `Vec::last` per value, to recover the pointer's provenance.
- `reset` freed every chunk, so an arena reused per task called the allocator again for every
  task.
- No huge pages, though [hardware.md](../../design/hardware.md) asks for 2 MB pages for arenas.

**Now.**
- The value goes down from the chunk's end with the same subtraction, mask and compare as
  bumpalo. `ptr` and `start` are pointers, and `with_addr` keeps provenance, so there is no
  `RefCell` on the fast path.
- The slow path is `#[cold]`.
- `reset` keeps the last chunk, so a task arena reset between tasks never calls the allocator
  once it has grown to the task's size. A test checks that `measure` sees 0 bytes
  over 100 resets.
- Chunks of 2 MiB or more are aligned to 2 MiB and given `MADV_HUGEPAGE` on Linux. That is a
  hint; the kernel's THP mode decides.

## `Cap` (`arena.rs`)

It is reserved once per chunk, so with chunks kept across resets it is off the per-task path.
Each `Grant` drop called `notify_all`, which with std's futex condvar is a `futex` syscall even
with no waiter. It now wakes only when someone waits. The wait itself stays, as core.md section
4 says a task that would pass the cap waits.

The lock is still a `Mutex`. A lock-free reserve would need a per-thread count of holders for
the "every holder waits" check, and it is not worth that while the lock is taken per chunk. The
lock audit's item 5 in `v2/requests/core.md` says so.

## Watching (blaze-core `fanotify.rs`, `watch.rs`, `fsevents.rs`)

**Benchmark.** Watchman on Linux keeps an inotify watch per directory and maps an event's watch
descriptor to its directory with one table lookup: nothing per event but that lookup and the
name. fanotify's `FAN_MARK_FILESYSTEM` needs no per-directory setup, which matters for cold
starts of large trees, but it names a directory by its file handle.

**Before (fanotify).**
- Every event of the whole file system was resolved with three syscalls (`open_by_handle_at`,
  a `readlink` of `/proc/self/fd/N`, then `close`), plus a `Vec` key and two `String`s.
- That counted events in `target/`, `/tmp` and every other directory beside the watched one,
  which a build writes thousands of times a second.
- The batch was deduplicated through a `BTreeSet`.
- A directory created and removed at once (a build's scratch directory) made the batch `lost`,
  which is a rescan of the whole tree. `learn_one` wrapped `ENOENT` in a message that dropped
  the raw error code, so `learn` never saw it as "gone".

**Now (fanotify).**
- A cache maps each directory's key to its place. The key is its file system id and
  `struct file_handle`, read in place from the event's bytes. The place is outside, under, or
  above the watched directory.
- An event in a known directory costs one `FxHashMap` lookup by the borrowed key. Events beside
  the watched directory (`target/`, `/tmp`) stop there, with nothing allocated.
- A handle is opened only the first time its directory is seen. The cache is dropped when any
  directory under the root moves and when events were lost; both can make a place untrue.
- New directories found by `learn` are put in the cache.
- A directory gone below the watched one is passed over (`gone()` keeps the raw error), not a
  rescan.
- Names go into one `Vec`, then sort and dedup: the same sorted, unique output as before.

This matches Watchman's per-event cost (one lookup), and keeps fanotify's setup of one mark per
file system. What is left per reported file is the `FileName` allocation that
`Done::Changed { files: Vec<FileName> }` asks for: an interface of core's.

**inotify and FSEvents.** inotify no longer clones its directory's `String` per event; FSEvents
no longer formats a `String` per path; both write into one reused buffer. Both use a sorted,
deduplicated `Vec` instead of a `BTreeSet`. FSEvents already watches only the directory, so it
had no fanotify-style flood.

## Backstops (blaze-core `cgroup.rs`, `footprint.rs`)

Both are per spawn, not per task. A limited spawn on Linux made about 12 syscalls for its
cgroup. Two of them were `stat`s of `memory.max` and `memory.peak`, whose answers are the same
for every cgroup the daemon makes. They are now checked on the first cgroup only, and a
`memory.max` missing later still gives the same "not enabled" error, from the write.

What dominates a spawn is the `fork` of the warm daemon in `process.rs` (core's).
core-linux's request 3 asks for `clone3` with `CLONE_VFORK | CLONE_PIDFD | CLONE_INTO_CGROUP`,
which would also remove the child's `cgroup.procs` write.

`footprint.rs` is already one shared sampler thread on a 2 ms tick, making one
`proc_pid_rusage` per live process per tick, and is unchanged.

## Tests added

- `arena.rs`:
  - `a_thread_adds_its_count_in_batches_and_the_rest_at_exit`: over 4,000 seeded allocations
    and frees, what is added plus what is not equals the thread's net; what is not stays under
    a batch; the exit flush adds exactly what is left, and after it every change is added at
    once.
  - `bumped_values_are_aligned_apart_and_kept`: 40 seeds of 3,000 values, sizes 0 to 200,000
    bytes, alignments 1 to 64, with random resets. Each value is aligned, apart from every
    other, and keeps its bytes. This is the model test for the downward layout.
  - `a_reset_arena_reuses_its_chunk_without_the_allocator` and
    `big_chunks_are_aligned_to_a_huge_page`.
- `fanotify.rs`:
  - `a_directory_is_placed_by_where_it_is_against_the_watched_one`.
  - `known_places_answer_until_a_directory_moves`: cached places answer with no file system
    marked; events beside the watched directory are dropped; events above it count only on the
    way to it; a directory moved drops the cache, so a later event is resolved anew.
