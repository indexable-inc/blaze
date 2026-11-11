---
status: current
summary: Which macOS interfaces blaze uses where a private, undocumented or Apple-only call beats the portable one (rule macos-private-apis), what each buys, its risk, and what is still to measure.
---
# macOS interfaces

Rule `macos-private-apis`: on macOS blaze uses a private, undocumented or Apple-only interface
where it is more efficient or more capable than the portable one, and the public one where
that is the better. Each use has a comment naming the symbol and why it wins, and a test that
resolves the symbol at run time (`dlsym(RTLD_DEFAULT, ..)`) and fails naming it. There is no
fallback to the portable call (rule `no-fallback`): a symbol blaze links against that the
system lacks stops blaze at launch, dyld naming the symbol.

This is the audit of 2026-11-10 (macOS 27.0.1, Apple silicon). "In use" rows are how the tree
is; "measure" rows name the measurement that decides them, below the tables.

## Parking and locks

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| `wait::Parker` (the pool's idle park) parked on std's `thread::park` behind a lock holding the `Thread` | `os_sync_wait_on_address_with_deadline`, `os_sync_wait_on_address`, `os_sync_wake_by_address_any`: a wait on the word's address | The compare and the block are one step in the kernel, as Linux's futex; a wake is one call and takes no lock; the same sans-IO core and DST as Linux (`crates/blaze-core/src/wait/parking.rs`). Time: to measure (M1) | Public header since macOS 14.4; older systems do not start blaze. Same on Intel | **In use**: `crates/blaze-core/src/wait/macos.rs:117` |
| the same | `__ulock_wait2` / `__ulock_wake` (private) | Nothing over the row above: `os_sync_*` is its documented face, with an absolute Mach deadline where `__ulock_wait2` takes a length | Operation numbers are XNU's to change | Not used: the public call is the better |
| `blaze_core::lock` is `parking_lot` (`crates/blaze-core/src/lock.rs`), whose macOS parker is a pthread mutex and condvar per thread | `os_unfair_lock` or an address wait under the lock's slow path | The fast path is one compare-and-swap either way; only a contended wait changes, and the unfair lock adds priority inheritance | A port of `parking_lot`'s parker, or of the lock | Measure (M2): only if `lock_waits` shows park time |
| `wait::sleep_until` | `mach_wait_until` to an absolute `mach_absolute_time` | No relative restart | Stable Mach call | In use: `crates/blaze-core/src/wait/macos.rs` |

## The event loop

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| One `kevent` a turn makes the turn's changes and takes its events (`crates/blaze-core/src/wait/macos.rs`, `kevent`) | `kevent64`, `kevent_qos` | Nothing: the extra fields (`ext[]`, a QoS per knote) are unused, and the call count is already one a turn | `kevent_qos` is private | Keep `kevent` |
| Hand-backs from file threads wake the loop with an `EVFILT_USER` trigger, once for all between two drains (`crates/blaze-core/src/kqueue.rs:157`) | a pipe | The trigger is already the cheaper: no descriptor, no bytes | none | In use |
| One driver thread | workloops (`kqueue_workloop_ctl`, `kevent_id`, `EVFILT_WORKLOOP`) | Kernel-managed threads with QoS overrides; nothing for one driver thread | Private, tied to libdispatch's internals | Not used; again if a second driver thread appears |
| A child's exit is an `EVFILT_PROC` knote with `NOTE_EXIT` (`crates/blaze-core/src/kqueue.rs:729`) | the same | No polling, no thread per child | none | In use |
| Memory pressure is an `EVFILT_MEMORYSTATUS` knote (-14) on the driver's kqueue (`crates/blaze-core/src/kqueue.rs:82`) | the same; libdispatch's memory-pressure source is this knote plus a queue | No dispatch queue, no hand-back; warn and critical levels (`EVFILT_VM` with `NOTE_VM_PRESSURE` is no filter any more: XNU maps it to `bad_filtops`) | Private filter number; tested | In use |
| No Mach ports are waited on | `EVFILT_MACHPORT` | Nothing to wait for | none | Not used |

## File operations

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| `Effect::Entries` listed with `readdir` and an `fstatat` per name | `getattrlistbulk`: names and attributes of as many entries as fit a 64 KiB buffer | One system call per some 500 entries in place of one per entry. Time: to measure (M3) | Public since 10.10. A name a volume is mounted on is stamped as the directory under it. A file system that returns no data length fails the listing, naming it | **In use**: `crates/blaze-core/src/fs/bulk.rs`, `crates/blaze-core/src/raw.rs:324` |
| `Effect::Stamp` is `fstatat` sweeps on threads (`crates/blaze-core/src/fs.rs`, `stamps`) | `getattrlistbulk` per directory | Only where a batch names most of a directory; a sparse batch reads entries nobody asked for | none | Keep; measure with M3 |
| A clone is `fclonefileat` from the source's descriptor (`crates/blaze-core/src/raw.rs:275`) | the same; `fcopyfile(COPYFILE_CLONE)` copies when it cannot clone | The descriptor form follows no swapped link; a copy in place of a refused clone is a fallback | none | In use; `fcopyfile` not used |
| Publish over a name is `rename` (atomic replace); publish if absent in the collector is `renamex_np(RENAME_EXCL)` (`crates/blaze-gc/src/fs.rs:1496`) | `renamex_np(RENAME_SWAP)` | A swap keeps the old file under the other name in one step; nothing blaze publishes needs the old one back | APFS and HFS+ only | Keep; `RENAME_SWAP` when a publish must keep what it replaced |
| `Durable::Yes` is `F_FULLFSYNC` (std's `sync_all`): the file, then its directory after the rename (`crates/blaze-core/src/fs.rs:169`, `crates/blaze-core/src/fs.rs:506`); `Effect::Sync` (`crates/blaze-core/src/threads.rs:1029`) | `F_BARRIERFSYNC` for the file's sync inside `write_atomically`, the directory's staying `F_FULLFSYNC` | Half the drive-cache flushes of a durable write: the barrier orders the bytes before the rename, and the directory's full flush that follows puts both on the media | Durability code: needs its design and a DST first (rule `dst-not-review`); barrier support is the drive's | Measure (M4). `Effect::Sync` and a rename's directory sync stay `F_FULLFSYNC` |
| `Durable::Started` (`Effect::WriteBack`) is `fsync` (`crates/blaze-core/src/fs.rs:227`) | the same | Bytes to the drive, no cache flush: what a cheap clone needs | none | In use |
| Large aligned writes and the store's reads set `F_NOCACHE` (`crates/blaze-core/src/raw.rs:355`) | `F_RDADVISE` / `F_RDAHEAD` for sequential reads | The store reads through its own pool with explicit reads, so read-ahead is the pool's | none | Keep; measure (M5) only if a cold pack read shows in spans |
| The flight recorder's post-mortem file is `F_PREALLOCATE`d (`crates/blaze-profile/src/flight/postmortem.rs`) | the same; `F_PUNCHHOLE` to give ranges back | Packs are immutable and removed whole; nothing punches holes | none | In use; `F_PUNCHHOLE` not used |
| Watches are by path (FSEvents); the journal resumes by event ID | `openbyid_np`, `fsctl`, `F_GETPATH`, `searchfs` | File IDs survive renames, but FSEvents already names what moved; `searchfs` is not on APFS's fast path | `openbyid_np` needs the volume's ID and is refused in the sandbox | Not used |

## Watching

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| One stream a watch with `FileEvents`, `NoDefer`, `WatchRoot`, latency 0 (`crates/blaze-core/src/fsevents.rs:693`) | `kFSEventStreamCreateFlagIgnoreSelf` | Drops events of the daemon's own writes | It would drop the watch's sync cookies, which the daemon writes itself: every `Effect::Flush` would hang until refused | Not used |
| the same | `kFSEventStreamCreateFlagMarkSelf` (events of this process carry `kFSEventStreamEventFlagOwnEvent`) | Could tell the no-op lane's echo (the walk's own writes under a directory of outputs) from a foreign write | It does not solve the echo: a build script's writes into `OUT_DIR` are a child's, not the daemon's, so they are not marked; and whether a foreign write coalesced with an own write of the same path keeps an unmarked event is undocumented, which is exactly the planted bug the lane's DST catches (`EchoHidesForeign`) | Not wired. Measure (M6) before any use |
| the same | `kFSEventStreamCreateFlagUseExtendedData` | The inode with each event | Needs `UseCFTypes`: a CFDictionary per event in place of a C string | Not used |
| A journal resumes `sinceWhen` a saved event ID, checked against the volume's UUID | the same | Restart without a scan | none | In use |
| the same | `/dev/fsevents` | No fseventsd between | Root only | Not used |

## Memory

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| The pool gives pages back with `MADV_FREE_REUSABLE` and takes them with `MADV_FREE_REUSE` (`crates/blaze-mem/src/os.rs`) | the same | Out of `phys_footprint` at once, the one measure (rule `one-memory-measure`); the region test holds the kernel to it | Apple-only advice | In use |
| Footprint is `proc_pid_rusage`'s `ri_phys_footprint` (`crates/blaze-core/src/raw.rs:437`, `crates/blaze-cargo/src/raw/memory.rs`) | `task_info(TASK_VM_INFO)`, `os_proc_available_memory` | The same ledger; `proc_pid_rusage` also reads a child's and the lifetime peak, which the others do not | none | In use |
| The daemon's ceiling is `memorystatus_control(SET_MEMLIMIT_PROPERTIES)` (`crates/blaze-cargo/src/raw/memory.rs`) | `task_set_phys_footprint_limit` | The same limit by another door, and kernel-private | Root or an entitlement either way | In use |
| A child's limit is a thread reading its footprint every 2 ms (`crates/blaze-core/src/footprint.rs`) | `posix_spawnattr_setjetsam_ext` (private, `spawn_private.h`) with a fatal memory limit at spawn | The kernel enforces it at the page fault: no polling thread, no overshoot | Private; may need an entitlement on macOS | Measure (M7) |
| The pressure signal is `EVFILT_MEMORYSTATUS` (above) | the same | | | In use |
| Copy-on-write views: the live patch remaps a written copy over read-only pages with `mach_vm_remap` (`crates/blaze-live/src/slots/macos.rs`) | the same | One atomic swap of pages other threads read | Mach call, stable | In use; its symbols have no resolve test yet |
| Caches are evicted by the pool on the pressure signal | `mach_vm_purgable_control` | The kernel drops volatile memory itself | Purgeable memory still counts in footprint while non-volatile, and a dropped range must be detected at every read: the pool's evictor already decides by footprint | Not used |

## Processes

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| `posix_spawn` with `POSIX_SPAWN_CLOEXEC_DEFAULT` and `posix_spawn_file_actions_addchdir_np` (`crates/blaze-core/src/process/spawn.rs`) | the same | No code of blaze's runs in the child; no descriptor leaks | `addchdir_np` since 10.15; now has a resolve test | In use |
| Children inherit the daemon's QoS | `posix_spawnattr_set_qos_class_np`, `posix_spawnattr_setprocesstype_np` | A class per child | Workers at `utility` got 2% of a core on a loaded host (2026-11-02) | Keep inheritance |
| Files a build script touches are attributed (TCC) to the app that started the daemon | `responsibility_spawnattrs_setdisclaim` | The child answers for itself | Private; a disclaimed child is asked for consent itself, with nobody to answer | Not used |
| A child's peak is `proc_pid_rusage` (`ri_lifetime_max_phys_footprint`) and its times `wait4`'s rusage | the same | Footprint, not RSS | none | In use |

## Sandbox

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| `sandbox_init_with_parameters` with a profile whose paths are parameters (`crates/blaze-cargo/src/sandbox/macos/raw.rs`) | the same | A profile applied in the child before exec, with no `sandbox-exec` process | Private; now has a resolve test | In use |
| Paths are granted by parameters of the profile | `sandbox_extension_issue_file` and `sandbox_extension_consume` | A grant after the profile is applied | The profile must allow the extension class, and the token is consumed by code inside the child: blaze runs none there | Not used |
| Nothing asks what the sandbox would allow | `sandbox_check` | A test could ask, without trying the access | Private | Not used; the sandbox's tests try the access, which is the evidence |

## Profiling and time

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| The sampler stops each thread (`thread_suspend`, `thread_get_state`, `thread_resume`; `crates/blaze-profile/src/sampler/macos.rs`) | kperf, `kdebug_trace` | Kernel-side sampling | kperf and ktrace need root | In use; kperf not used |
| A span's CPU is `clock_gettime(CLOCK_THREAD_CPUTIME_ID)` (`crates/blaze-profile/src/lib.rs`) | `thread_selfcounts` (private) | Instructions and cycles a span beside its time | Private, its struct versioned | Measure (M8) |
| Monotonic time is std's `Instant` (`mach_absolute_time` underneath) | `mach_continuous_time` | Counts sleep; a build's deadlines should not | none | Keep |
| Workers run at `user-initiated`, or `default` when lowered (`crates/blaze-graph/src/pool.rs`, `worker_qos`) | `pthread_set_qos_class_self_np` to `utility` or `background` around a job of the plane's idle class | Idle work on efficiency cores | A fiber resumes on any worker, so the class must be set and restored per job; `utility` starved on a loaded host | Measure (M9) |
| Nothing reads the CPU number | `pthread_cpu_number_np` | Nothing blaze keys on | none | Not used |

## Code memory

| Today | Better call | Buys | Risk | Decision |
|---|---|---|---|---|
| The dev run's code memory is `MAP_JIT` with `pthread_jit_write_protect_np` per thread and `sys_icache_invalidate` after a write (`crates/blaze-inproc/src/exec.rs`) | the same | The only way to write and run pages on Apple silicon without a signature | Hardened runtime needs the JIT entitlement | In use: right |
| The image's read-only pages are never made writable: a copy is remapped over them (`crates/blaze-live/src/slots/macos.rs`) | `mach_vm_protect` in place | A window where another thread faults | | In use: right |

## To measure

Each on the Mac, on one named commit before and after (rule `measure-the-exact-tree`).

- **M1, the park**: two threads handing a token back and forth through `wait::Parker` a
  million times, wall time and system calls, on the commit before the address wait and on it;
  and a warm self-build's park spans (count and total) from its profile.
- **M2, contended locks**: `blaze debug locks` on a self-build; a port is worth it only if
  park time (not hold time) is over 1% of worker time.
- **M3, listing**: `Effect::Entries` of a directory of 10,000 files and of the checkout's
  largest directory, cached, before and after; then the same directory through
  `Effect::Stamp` to see whether a full-directory batch should take the bulk call too.
- **M4, the barrier**: from a cold self-build's profile, the count of `Effect::Write`s and
  the time inside their two syncs; if over 1% of the build, design the barrier level (a
  `Durable` between `Started` and `Yes` for the file's sync inside a durable write) with its DST.
- **M5, sequential reads**: a cold read of the largest pack with and without `F_RDADVISE` of
  the range, timed by the pool's load spans.
- **M6, MarkSelf**: a stream with `kFSEventStreamCreateFlagMarkSelf` (0x20) on a temporary
  directory; this process writes a file, `/bin/sh` writes another, then both write the same
  file within a millisecond, a thousand rounds. `kFSEventStreamEventFlagOwnEvent`
  (0x0008_0000) must be on every event of the first, on none of the second, and each round of
  the third must hold an event without it. One round that fails is the answer: not usable.
- **M7, a child's limit**: `posix_spawnattr_setjetsam_ext` with its fatal-limit flag and
  active and inactive limits of 64 MiB (signature and flags from XNU's `spawn_private.h` of
  the running release) on a child that touches 256 MiB, as the user that runs blaze: is the
  spawn accepted, and is the child killed near 64 MiB.
- **M8, counters**: the cost of one `thread_selfcounts` against one
  `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, a million calls each.
- **M9, idle class**: a speculative build in the plane's idle class beside a foreground
  build, workers at `default` against the idle job's worker at `utility`: the foreground's
  wall time and the idle job's progress.
