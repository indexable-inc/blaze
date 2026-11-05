---
status: plan
summary: blaze v2 restore on Linux: the statx sweep of tracked paths on a per-core pool beside blaze-core's ring, its speed of light, and the journal effect on fanotify or inotify.
date: 2026-10-29
---
# Changed files on Linux: a statx sweep on every core, then a live watch

Part of [restore](../../../design/restore.md) ("Changed files come from the OS's journal").
The code is `crates/blaze-core/src/uring.rs`; the types are in `crates/blaze-core/src/journal.rs`.

## Linux

Linux keeps no journal of changes across a restart. A warm restart therefore stamps every tracked
path once and compares each stamp (inode, size, mtime, ctime, mode) with the column saved in the
image. Only paths whose stamp moved are hashed, and the directories whose stamp moved are listed.
Live changes come from the watch from then on.

### `Effect::Stamp` on the stampers

- **A pool of threads, one per core** (`Stampers`), made at the driver's first sweep and kept for
  its life. A sweep is one `Job`: the names, one output vector of a stamp per name, and two
  counters.
- **Fan-out as a tree.** The ring's thread sends the job to stamper 0 only. Stamper `i` hands it
  to `2i + 1` and `2i + 2` before it starts work, and only as far as the job has chunks for, so
  every core is at work after log2(cores) wake-ups and a small sweep wakes few threads.
- **Chunks of 64 names**, claimed with one `fetch_add` each. Each stamper writes its chunk's
  stamps into the shared vector by index, so the stamps come out in the names' order. The
  stamper that counts the last finished chunk (an `AcqRel` add, so every writer's stores are
  seen) takes the vector, sends `Done::Stamped` down the offload channel and wakes the ring's
  eventfd. The completion comes out of `poll` like any work done off the ring.
- **One `statx` per name, relative to its directory.** Within a chunk, a run of names in the same
  directory opens that directory once with `O_PATH | O_DIRECTORY` (which needs only search
  permission, as a walk of the whole name does) and stats each leaf against it, so the kernel
  walks one component per name and not the whole path. Flags are `AT_SYMLINK_NOFOLLOW |
  AT_STATX_DONT_SYNC`, and the mask is `STATX_TYPE | MODE | INO | SIZE | MTIME | CTIME`. Names are
  `Names`' bytes in place, already NUL-terminated, so nothing is allocated or copied per name.
- **Stamps** are `crate::fs::stamp_of`'s: `sec * 1e9 + nsec` wrapping, with `mode` taken from
  `stx_mode`. A name with nothing there (`ENOENT`, `ENOTDIR`) is `Stamp::GONE`. Any other error,
  or a `statx` that did not fill a field asked for, is `Stamp::UNKNOWN`, so the asker looks at
  that name and meets the error itself. A directory that cannot be opened gives each of its names
  what a `statx` of the whole name would (gone for `ENOENT` and `ENOTDIR`, unknown otherwise). The
  effect as a whole fails only when the root cannot be opened.
- **One known difference** from `fstatat` of the whole name: a name longer than `PATH_MAX` whose
  directory and leaf are each shorter is stamped where `fstatat` says `ENAMETOOLONG` (unknown).
  Cargo's paths do not reach 4096 bytes; the tests do not cover it.
- **Drop.** Dropping the driver sends the stampers their end after the jobs before it; a job in
  flight finishes into a channel nobody reads and is freed with its last stamper's reference.

### Speed of light

The work is fixed by the problem: for each of N tracked names, the kernel must find its inode
and copy out five fields. There is no batched `stat` system call on Linux other than io_uring's,
which is one request per name too (see below). The numbers are for N = 100 000 names in
directories of about 100, with the dentry and inode caches warm.

- **Kernel work per name:** an RCU walk of one component in a known directory (~0.1–0.3 µs), the
  `statx` fill and its 256-byte copy-out (~0.1 µs), and system call entry and exit (~0.1–0.4 µs,
  depending on the CPU's mitigations). That is t ≈ 0.4–1 µs, about 0.05–0.1 s of one core for
  100 000 names. The benchmark measures t on the machine it runs on instead of trusting these
  numbers.
- **Over the cores:** N·t / C. With t = 0.5 µs that is 100 000 × 0.5 µs / 8 = **6.3 ms on 8
  cores** and 100 000 × 0.5 µs / 64 = **0.78 ms on 64**. The dcache's RCU walk takes no lock or
  reference count on the leaf, so the work scales with cores.
- **Memory:** the names (~30 bytes each, 3 MB) are read once and the stamps (40 bytes each,
  4 MB) are written once: 7 MB at ~50 GB/s is 0.14 ms, under the compute bound on 8 cores and
  under it by 5× on 64. The kernel's `struct statx` is written to the same stack slot every time,
  so it stays in L1.
- **System calls:** the minimum is one per name. The sweep adds one `openat` per run of names in
  one directory per chunk: with directories of 100 and chunks of 64, about 100 000 / 64 + 1 000
  ≈ 2 600 opens, 2.6% of the calls.
- **Critical path:** the fan-out is log2(C) wake-ups of ~5 µs (30 µs on 64 cores), and the cores'
  last chunks end within one chunk of each other (64 × 0.5 µs ≈ 32 µs). Together about 60 µs, 1%
  of the 8-core bound and 8% of the 64-core one. Chunks of 512 would leave 256 µs of tail, which
  is 33% on 64 cores; that is why the chunk is 64.
- **Design target:** 90% of N·t / C, so at most 7 ms on 8 cores and 0.87 ms on 64, against the
  restore doc's 50 ms budget.

What stood in the way, and how it was removed:

- **io_uring's `STATX` cannot reach it.** The kernel always punts `STATX` to an io-wq worker
  (`REQ_F_FORCE_ASYNC`). Each request is prepared on the one submission thread (SQPOLL, or the
  caller's `io_uring_enter`), which copies the name in (`getname`) and queues and wakes a worker:
  about 0.3–0.6 µs per request, serially. For 100 000 names that is 30–60 ms on one thread,
  whatever the number of cores: 5× the 8-core bound and 40–80× the 64-core one, before any
  worker runs. Each request also walks the whole name from the root. The first design used the
  ring and was replaced by the stampers for this reason.
- **The threaded reference** (`crate::fs::stamps`) uses at most 8 threads, spawned per call, and
  `fstatat` of whole names. It is a correctness oracle, not a candidate.
- **What remains:** system call entry and exit is part of t and cannot be removed without a
  batched `stat` in the kernel. Contention on a directory's reference count, when several cores
  open the same directory with `O_PATH` at once, is bounded by the 2.6% of calls that are opens.
  Neither is expected to cost 10%; the benchmark checks it.

### `Effect::Entries` and `Effect::Journal`

- `Entries` runs `crate::fs::entries` on one stamper, the stampers taking listings in turn,
  because io_uring has no `getdents`. A thread of its own per listing (as `List` still has)
  costs ~20 µs and a fresh stack to start, as much as listing a directory of 100 names, and a
  restore lists every directory whose stamp moved.

### Allocations

- Per name: none. `stamp_names` writes into the job's one result vector and reuses each
  stamper's directory-name buffer; `uring::tests::a_sweep_allocates_per_sweep_not_per_name`
  holds it to zero.
- Per sweep, on the ring's thread (a restart's work, not the warm loop's): the root's name as a
  C string, the job's `Arc`, its result vector, and now and then a block of the channel to
  stamper 0. The same test holds a submit to at most 4, whatever the number of names.
- Per listing (the warm loop's, for a directory whose names changed): nothing on the ring's
  thread but, now and then, a block of a stamper's channel (at most 1, tested); the listing's
  own result is its stamper's.
- The wake's read of the eventfd reads into one buffer the driver keeps for its life (an
  `Rc` that a request the kernel still holds keeps alive), so a wait allocates nothing.
- Against [memory.md](../../../design/memory.md): the result vectors are the asker's
  (`Done::Stamped`, `Done::Entries`) and go back to the general allocator until `Done` can
  carry a request arena's or a pool's memory. That is blaze-mem's lane and the `Done` type's
  owner's call; nothing here holds it back.
- The driver's root is an `Arc<Path>`, so each effect's and each piece of work off the ring's
  handle of it is a count, not a copy of the path.
- `Journal` is a watch (`crate::watch::Tree`: fanotify, or inotify without the privilege). Its
  first completion is `journal::first_on_linux`, which says the changes since any saved cursor
  are `Lost::NoJournal`. Each batch the tree reads after that, including those drained by a flush,
  goes through `journal::live`. `Done::Journaled` does not end the effect, and `Unwatch` ends a
  journal as it ends a watch.

### The benchmark

- **Target:** both the restore doc's budget of about 50 ms for 100k tracked files on a warm
  restart (the dentry and inode caches holding the tree, as after a daemon restart without a
  reboot) and 90% of the speed of light above.
- **State of the art:**
  - Watchman's initial crawl does a `readdir` plus `lstat` per entry on a thread pool, and pays
    for it on every start that has no saved clock. Its warm query is fast only because the process
    stayed up.
  - `fd` and ripgrep (the `ignore` crate's parallel walker) avoid `stat` wherever `d_type` is
    enough, and walk a hot tree of 100k entries in tens of milliseconds over all cores.
- **What beats it:** no walk at all. The tracked paths come from the image, so no `getdents` is
  needed, and each directory is opened only to stat its names. Directories are listed only where
  their own stamp moved.
- **Measured by:** `uring::tests::a_sweep_of_100k_names_is_within_the_restart_budget`. It is
  ignored by default because it makes 100k files. It takes the best of five warm sweeps, and it
  measures the bound on the same machine: one thread's plain `statx` calls of every whole name,
  best of three, divided by the cores. It asserts the sweep under 50 ms and within 10% of that
  bound, and prints the threaded reference's time beside them. No number is recorded here until
  the coordinator runs it.
- **Cold cache** (after a reboot): each `statx` may read an inode from disk, and the bound
  becomes the device's random-read rate. With one blocking call per core, C reads are in flight;
  this is outside the 50 ms budget, which is for restarts.

### What the tests prove

- `a_sweep_stamps_as_the_reference_does`: 10 000 files over many chunks and directories, plus a
  file at the top, directories, a symlink to a file, a dangling symlink, a symlink to a directory
  with names through it, a directory that can be searched but not read with a file in it, an
  executable file, and names with nothing there (under a file for `ENOTDIR`, under a dangling
  link, through the directory's link). Name by name, `Done::Stamped` equals `crate::fs::stamps`,
  the safe reference, and nothing is left in flight.
- `a_sweep_allocates_per_sweep_not_per_name`: the allocation budget above.
- `sweeps_in_flight_together_and_with_other_effects`: two sweeps at once, one of them in reverse
  order, plus an empty sweep, an append, a timer and two `Entries` (one of a directory that is not
  there). Each ends as the reference has it. A sweep of a missing root fails, naming the root.
- `a_journal_tells_what_is_lost_then_what_is_written`: with a saved cursor, the first batch says
  `NoJournal` for the directory, and with `Cursor::None` it says nothing is lost. A file written
  afterwards is told as a live `Journaled` before a flush asked after it, and `Unwatch` ends the
  journal with nothing left in flight.
