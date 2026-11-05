---
status: plan
summary: blaze v2 core as written on 2026-10-29: the sans-IO core and its drivers (io_uring, kqueue, IOCP, simulation), the plane (single flight, fair share, critical path, memory admission, no memo of its own), the worker pool and pump-wait, memory admission with its hard backstop, and a build's nodes.
date: 2026-10-29
---
# blaze v2: core, as written

What lane core wrote on 2026-10-29 for [core.md](../core.md), where each piece lives and what
the other lanes wire to it. Nothing of it had been built or run when this was written; the
first build is the coordinator's. It builds on [driver.md](../../../../driver.md) (steps 1 to 3
on main) and on [#283](https://github.com/indexable-inc/blaze/pull/283)'s sans-IO core.

## The shape

    request (a view: a build, a test run, an edit)      many at once, from any client
        │  nodes, each keyed by the hash of its inputs
        ▼
    blaze-graph::exec::Scheduler   owner thread of the plane; measures, posts, reposts
        │                          the database's Memo: a key done before runs nothing
        ▼
    blaze-graph::Plane      sans IO: single flight, fair share between requests, critical
        │                   path within one, memory admission
        │  Run { node, pool, near }
        ▼
    blaze-graph::pool       one worker per CPU, grouped by CCD or cluster, pinned,
        │                   stealing nearest first; pump-wait inside a task
        │  IO effects
        ▼
    blaze-core::Driver      io_uring (Linux), kqueue (macOS), IOCP (Windows), Sim (tests),
                            Threads (portable reference)

Nothing above `blaze-core`'s drivers does IO; nothing above the pool starts a thread; the plane
reads no clock. Each layer is tested by itself, the plane and the core under a seeded
simulation over many seeds.

## The IO core (`blaze-core`)

Effects with tags, completions and a `Driver` (`submit`, `poll`, `in_flight`, `now`). Every
driver does every effect; `tests.rs` runs the same scripts on each and compares what they end
with.

| effect | Linux (`uring.rs`) | macOS (`kqueue.rs`) | Windows (`iocp.rs`) | `Sim` |
| --- | --- | --- | --- | --- |
| read, read at, append, sync, truncate | the ring | file threads, `F_FULLFSYNC` | the port | in memory, a log of unsynced changes |
| write at an offset | the ring, on a descriptor without `O_APPEND` | file threads | the port | in the log |
| whole-file write (temporary, sync, rename) | the ring + `renameat` | file threads | file threads, `MoveFileExW` written through | all or nothing over a crash |
| rename, mode, stat, list, remove, link | where submitted, or a thread for a list | file threads | file threads | in memory |
| clone | `FICLONE` | `clonefile` | ReFS block cloning | shares the bytes |
| map | `mmap`, `MADV_HUGEPAGE` from 2 MiB | `mmap` | `MapViewOfFile` | the bytes |
| timer | the ring | `EVFILT_TIMER` | the port's wait, to the next due | the virtual clock |
| watch | fanotify, inotify when fanotify is refused for privilege (`EPERM`) | FSEvents | `ReadDirectoryChangesW` | changes the test makes |
| spawn, kill | `pidfd`, the ring polls it and the pipes | `EVFILT_PROC`, `EVFILT_READ`/`WRITE` | a job object each; its end from a thread-pool wait on the process, posted to the port | a program the test gives |
| listen, connect, recv, send, hang up | the ring | `EVFILT_READ`/`WRITE` | Winsock on the port | pairs of ends in memory |

A clone where the file system cannot clone is an error that names it: no copy. A file system
that gives no file handles is an error for fanotify, not a switch to inotify; only a refusal
for privilege is (no-fallback). `Threads` is the portable reference: blocking calls on
threads, the meaning every other driver is compared with.

**The ring** (Linux 6.0 or newer, else an error naming the feature): SQPOLL, so a busy driver
submits with no system call; a sparse registered-file table for cached files and sockets, its
upper half for multishot accepts straight into slots; four registered 1 MiB buffers for
`O_DIRECT` writes; multishot receives from a provided-buffer ring; a whole-file write as linked
write, sync, rename and directory sync. A `WriteAt` of 1 MiB or more whose offset and length
fit the device's `STATX_DIOALIGN` bypasses the page cache (`F_NOCACHE` on macOS, from 4 KiB
alignment); the bytes on disk are the same either way.

**Sandboxes are usercode's.** A `Command` with `Sandbox::On` runs through a
`blaze_core::Confine` the driver was given (`Threads::confined`, `Uring::confined`,
`Kqueue::confined`, `Iocp::confined`): it says the program, arguments, environment and
directory that run the command confined, with its trace under a scratch directory
(`.blaze-spawn/<pid>-<n>`, removed after), and reads the trace back as `Traced`. A driver
given no `Confine` fails every sandboxed spawn naming it.

**The hard backstop for a process.** A `Command` with `memory: Some(bytes)` runs under a limit
the kernel holds: on Linux a cgroup v2 of its own (`memory.max`, swap 0, the child joins it
before `exec`; its `memory.peak` is the `Exit`'s peak; Linux 5.19 or newer, else an error
before it runs); on macOS a watch of its physical footprint that kills it past the limit
(macOS refuses `RLIMIT_AS` and `RLIMIT_DATA` below what every process reserves, so none is
set); on Windows its job object's
memory limit, which refuses any commit past it (so its peak is at most the limit; the job's
port messages, which Windows does not promise, only say it went over). The watch stops before
the process is reaped, so no kill reaches a reused pid.

**No process starts by `fork`** (`process/spawn.rs`). Everything the child needs (paths,
argument and environment arrays, pipes past descriptor 2, the cgroup's `cgroup.procs` open) is
made in the parent first. On Linux `clone(CLONE_VM | CLONE_VFORK | CLONE_PIDFD)` runs the child
on a stack of its own in the daemon's memory, with every signal blocked across it: the child
resets its handlers, takes its pipes, writes itself into its cgroup, changes directory and
`execve`s, using system calls alone; a step that fails comes back as the spawn's error naming
it. The pidfd comes from the same call, so it names this process and no later one. On macOS
`posix_spawn` with every descriptor not given closed (`POSIX_SPAWN_CLOEXEC_DEFAULT`). A spawn
costs the same however large the warm daemon is. The program is looked up on the `PATH` of
its own environment, the environment ordered by name with a later value replacing an earlier
one, and SIGPIPE back to its default, as std's spawn does.

**The simulation's files** keep a log of changes since their last covering sync (appends,
writes at an offset, truncations): a crash keeps a random prefix of it, the last change maybe
torn. A sync covers the changes made before it was asked.

## The plane

One plane per daemon. A request adds nodes, each with a key (the hash of what its value depends
on, [unit-keys.md](../../../../unit-keys.md)) or none, its deps, its pool, its expected cost and
its expected memory.

1. **No memo of its own.** A node whose key the query database has settles at once from it
   (`Memo::settled(key)`, given to `Plane::with_memo`), `Cached { remote: false }`, and runs
   nothing. A task's value must be in the database before its `Done`.
2. **Single flight.** A node whose key is running or waiting joins it: one task runs, every
   request that asked settles when it does. A task not started is ready in the request of each
   of its nodes, at that node's priority, and whoever starts it runs it.
3. **Fair share.** Stride scheduling: the next thread of a pool goes to the request that
   started the fewest tasks of it for its weight. A request that had no ready work comes back
   at the plane's virtual time; ties go to the heavier, then the first opened. Work under way
   is never stopped.
4. **Critical path.** Within a request, the ready node with the longest path of expected cost
   to the end runs first; ties to the node added first. A `Run` says `critical` when its
   node's path is at least that of every ready and running node of its request, and the pool
   puts it on a fast group.
5. **Memory admission.** A task starts only when the granted memory plus its own prediction
   fit the budget. A task passed over `starve_after` times holds back every task with memory,
   of every pool and request, until it starts (tasks with no memory still start). A task
   whose prediction alone is over the budget fails before it starts, naming itself, its
   prediction and the budget.
6. **Cancel and release.** `Plane::cancel(request)` skips what of it has not started, lets what
   runs finish, and ends it not ok. `Plane::release(request)` gives back a finished request's
   room. `Plane::stats(request)` says what it ran, found cached and joined, and its critical
   path as measured.
7. **Layout.** Nodes and tasks are `u32` slots into hot rows (64 bytes a node, 32 a task) and
   colder columns, reused through free lists; a request's deps are one edge array (a node's
   deps a range of it, its dependents a list threaded through it); task members a list through
   the node rows; ready sets 4-ary heaps whose nodes know their place; keys index the flight
   table by their own first eight bytes. In steady state a task costs the plane no allocation;
   `Plane::step_into` appends to the caller's buffer.

`Machine` (the build-script and name-resolution graphs) is the plane with one request and no
memo; its outputs are the same as before, which a test checks over a recorded trace and 300
random builds. The plane's tests drive random graphs of up to five requests over 600 seeds and
check after every step that nothing runs before its deps, no key runs twice at once, granted
memory stays within the budget, the held task starts first, and every request ends once.

## Memory admission (`admission`, `arena`)

- **Estimates** per task kind: a peak at or over the prediction becomes the prediction at
  once; 64 peaks in a row under it bring it half way down to their most. Unseen kinds take a
  default. Encoded deterministically for the daemon's image.
- **Measured, not guessed**: `arena::Counting` is the daemon's global allocator, over the
  allocator it uses (per-thread counts added to one total each time they drift 256 KiB and at
  thread exit: no lock and no atomic per allocation; `held()` is one load, within 256 KiB per
  other thread); `measure(f)` is the peak of what `f` held, `held()` what the
  process holds. Blaze's own memory is `own()`: `held()` plus every blaze-mem region's
  committed bytes (`blaze_mem::tag::snapshot().committed`), what the scheduler's `Held` reports
  after each run. The scheduler measures every task and feeds `observe` (a task's frames join
  its peak once blaze-mem can measure them, docs/v2/plan/lanes/mem/core.md).
- **Subsystems**: each task kind maps to a blaze-mem tag (`admission::tag_of`), and each tag's
  budget is its share of the machine budget in 64ths (`admission::share`, `set_budgets`). A run
  whose tag is over budget waits while another run of that tag is under way (`exec::Throttle`),
  so a runaway subsystem is held back before the machine runs short, one task at a time, never
  stopped.
- **The daemon's own cap**: `Bump` arenas reserve their chunks from a `Cap`, which calls a
  spill (cold results to disk) once each time it crosses `spill_at`, makes a reserve over the
  limit wait, and when every holder waits and nothing fits cancels the newest waiter
  (`Cancelled`, reposted), so the cap never deadlocks.
- **The budget**: `machine_budget(margin)` is the least of the cgroup's `memory.max` (and its
  ancestors'), `MemTotal` and `RLIMIT_AS` (`hw.memsize` on macOS), less what others use in
  blaze's own cgroup (`memory.current` less `own()`), less the margin.

## The executor (`topology`, `pool`, `pump`, `exec`)

- **Groups**: on Linux the CPUs of a level 3 cache (an EPYC's CCD) or, where smaller, a
  cluster of more than one core (Graviton); on macOS an L2 cluster of one performance level,
  level 0 fast. A group never spans NUMA nodes (an L3 shared across sub-NUMA clusters is split
  by node). Only CPUs in the affinity mask. Stealing goes to the own group, then its node, then
  its socket, then the rest. Where sysfs, the affinity mask or the perflevels say nothing,
  `Topology::detect` is an error naming what, never one group.
- **Pool**: one worker per CPU, pinned on Linux (`sched_setaffinity`), a QoS class by P or E
  core on macOS; a lock-free deque per worker and an injector per group; a job near no group
  goes to the least loaded group, fast first. A worker that cannot start or bind fails the
  pool: it is never quietly smaller.
- **Pump-wait (#1301)**: a task waiting for another's key (`pump::Flights::get_or_run`) runs
  other work meanwhile, up to a depth of 16. One wait graph for the process: the wait that would
  close a cycle is `Cancelled` at once and its task posted again, so of any cycle one task gives
  way and none deadlocks. A key asking for itself is a real cycle and panics naming it.
- **Scheduler**: owns the plane on its own thread; runs work tasks on the shared pool and IO
  tasks on IO threads, inside `measure`; raises each spec's memory to its kind's prediction;
  reposts `Cancelled` work where it ran. `drive` (the one-request path) runs on two pools lent
  to it alone and kept for the next build of their size, so a build starts no threads.

## A build's nodes (`build`, driver.md steps 4 to 7)

`build::Build` says a build as plane nodes: the manifest and fetch (IO), std, build scripts and
their runs, proc macros, names and bodies, and per unit a compile (work, keyed by its code hash)
followed by its store write (IO). A link waits for the compiles and std, not their store
writes, so writes overlap the next compiles (step 5); links and test runs are nodes (step 6);
an input committed while work runs is a new view and a new request, and the old one is
cancelled (step 7).

## What others wire

- usercode: a `Confine` over the sandbox, given to the drivers.
- the daemon: `arena::Counting<Jemalloc>` (or mimalloc) as its global allocator; its work on `exec::Scheduler` with the
  database as its `exec::Runner` (each task's work is data the runner reads, not a closure) and
  its `Memo`, both type parameters, the budget from `machine_budget`.
- db: `blaze_query::Cancelled` as `blaze_graph::Cancelled`, `Cx::get` on `Flights`.
- ci: `NodeState::Cancelled`, and `Plane::stats` in the API's per-request work.

## Rules that hold throughout

No rustc; no fallback (a node or effect that cannot run fails and names itself); every key
covers every input; build scripts and proc macros sandboxed; semantics in the front end's
crates, the plane, the executor and the drivers outside it.
