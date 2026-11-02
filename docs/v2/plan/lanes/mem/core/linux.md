---
status: plan
summary: mem-core-linux - the Linux driver (uring.rs), its watches (watch.rs, fanotify.rs) and the memory backstop (cgroup.rs) onto blaze-mem - requests, kept files, watches, spawns and connections in Slabs tagged Tag::Io with the request's handle as the SQE's user_data so a stale completion is told and dropped, directory tables as spans in one column, paths built in reused buffers, each warm operation's allocations held to its payload by a test, and each operation's cost against its bound.
date: 2026-10-29
---

# mem-core-linux: the Linux driver onto blaze-mem

Child of [mem-core](../core.md). Paths: `crates/blaze-core/src/uring.rs`,
`crates/blaze-core/src/watch.rs`, `crates/blaze-core/src/fanotify.rs`,
`crates/blaze-core/src/cgroup.rs`. Nothing was built or run; the tests are written beside the
code for the coordinator's run.

## What moved where

| Was | Is | Held by |
| --- | --- | --- |
| `pending: FxHashMap<u64, Pending>`, a counted `u64` id per request | `Slab<Pending>` (`Tag::Io`, at most `PENDING = 1 << 20`); `user_data` is the handle's generation, then its index and a 2-bit link | `pending_agrees_with_the_map_it_replaced` (model test, 64 seeds of 2000 steps) |
| a completion found by id, or not | a completion whose handle's generation is not its slot's is stale and dropped; an op is stepped in place (`Op::Stepping` while it is), so it keeps its handle | `a_completion_of_an_ended_request_is_stale` (a sync's slot taken by a read; an unwatched watch's read's slot taken by the next watch) |
| `files: FxHashMap<FileName, Rc<Fixed>>` | `kept: Slab<Kept>` (descriptor, users, named) and `names: FxHashMap<FileName, Handle<Kept>>`; a request holds a handle, not an `Rc` | `kept_files_are_registered`, `a_warm_cycle_allocates_the_read_alone` |
| `abandoned: Vec<Pending>` after a ring failure | the request stays in its slot, marked abandoned, so the kernel writes into nothing else; its completions are dropped | `a_driver_dropped_with_work_in_flight` |
| watches, listens, spawns, connections in maps by tag or counted id | `Slab<Watching>`, `Slab<Spawning>`, `Slab<Connection>`, a tag-to-handle map where an effect names a tag; `Conn` is a handle's bits, an even generation mapped to no slot | the driver's tests, each ending with every slab empty; a forged `Conn` fails |
| a `CString` per path | the path joined in `blaze_mem::scratch`, NUL-terminated in reused bytes; the two names of a write or rename in a pooled `Vec<u8>` | `a_warm_cycle_allocates_the_read_alone` |
| a pipe, a watch's read buffer, a tree's tables made per spawn or watch | pools of `SPARES = 64` buffers and spare watch rooms | `a_warm_watch_allocates_its_changes_alone` |
| inotify's `dirs: FxHashMap<i32, String>` | spans into one `Strings` column, squeezed when holes are most of it | `strings_squeezed_keep_every_name_kept` |
| fanotify's `known`, `seen`: `FxHashMap<Box<[u8]>, ..>` | `Dirs`: keys and names as spans of one column, entries in a `RegionVec`, a free list; keys and file handles on the stack | `dirs_agree_with_a_map_of_owned_keys` (32 seeds of 4000 steps, then a warm block of 0 allocations) |
| a batch's names as a `String` each | spans into one reused `String`, sorted and deduplicated before any `FileName` is made | `a_batch_is_deduplicated_before_its_names_are_made`, `a_walk_finds_what_read_dir_finds` (a warm walk in 0 allocations), `a_warm_watch_allocates_its_changes_alone` |
| a cgroup's `PathBuf` and `CString`, its parent cloned, each control file's path joined, the limit formatted | one `Vec<u8>` (directory, then `/cgroup.procs\0`) from a pool of 64; the parent in a `OnceLock`; control-file paths in scratch; the limit and `memory.peak` in stack buffers | `a_process_is_held_to_its_limit` (a warm cgroup made, limited, read and ended in 0 allocations) |

Timers were already on the wheel: `a_warm_timer_allocates_nothing` holds it.

## Speed of light

Each operation's floor is the kernel's own work. The driver's part is one 64-byte SQE written,
one 16-byte CQE read, and the request's slab entry (`Pending`, about 200 bytes, so three or four
cache lines) touched at its start and at each completion. There is no allocation and no syscall of
its own: one `io_uring_enter` submits a batch and waits for its completions.

- **A file op** (Sync, Stat, Read of a cached file): a cached read completes inline in the kernel
  in about 1 us. A sync goes to an io-wq worker (a thread hop, several us) and then waits on the
  device, which dominates. Stat runs `statx(2)` on the ring thread, about 1 us. Against these, the
  driver's part (a slab insert, a lookup, a remove and two copies of the entry for the in-place
  step, tens of ns) is under 5%.
- **A timer**: a wheel insert and a pop, O(1). The next deadline is `io_uring_enter`'s timeout,
  so there is no SQE of its own.
- **A watch event**: one read of the notification descriptor per batch, then one span per name,
  one unstable sort, and one `FileName` per distinct name (the payload). Each new directory under
  the watch costs the kernel's `inotify_add_watch` or fanotify mark, as it must.
- **A connection send or receive**: one SQE each. Receives are multishot into a provided buffer
  ring, so a connection's receive is armed once and each arrival is one CQE. The received
  `Vec<u8>` is the payload.
- **A cgroup**: `mkdir`, three writes, then at its end a kill, a poll on `cgroup.events` and an
  `rmdir`. Those are the kernel's cost; the driver adds a mutex taken twice for the path pool.

How it stays within 10%: the driver's per-operation work is a constant number of cache lines
and no allocation, against a kernel floor of a microsecond or more. The budget tests hold the
allocation count. What is not yet measured is the time per operation against a bare io_uring
loop doing the same SQEs. That is the next number to take, in an ix VM.

## Caveats

- Stepping in place copies `Pending`'s op out and back, two copies of about 200 bytes per
  completion. That is safe because every buffer lent to the kernel is on the heap, except a
  connect's inline `SockAddr`, and a connect has one completion. Shrinking `Op` (moving
  `Write`'s and `Rename`'s fields behind a pooled entry) would make it one cache line.
- The stamper's and offload's channels are unbounded. A bounded channel would block the ring
  thread, and the existing budgets codify these paths as they are.
- `process::start` (not this lane's path) still allocates per spawn: the `Command`'s argv and
  environment, and its `Finish`.
- `Slab::get` trusts an odd generation (request in `v2/requests/mem-core.md`, from
  mem-core-macos). Until that is fixed, `conn_of` maps a caller's even generation to no slot.
  Every other handle here is one the driver issued.
