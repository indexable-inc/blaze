---
status: plan
summary: blaze v2: the sans-IO core, drivers, the scheduler and memory admission.
date: 2026-10-29
---
# blaze v2: core

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 3. Sans-IO core and drivers

From blaze-next's `blaze-core` and the `sansio` lane's steps 1-3 of `docs/driver.md`
(on main), finished:

- The compiler core is pure: it takes completions, returns effects (read, write, sync, spawn,
  watch, timer). No thread, file, socket or clock in it.
- **Drivers**:
  - Linux: io_uring (reads, writes, fsync, timers, accept and recv for the API, linked
    submissions), fanotify for file changes, `pidfd` for children, Landlock + seccomp +
    namespaces for sandboxes.
  - macOS: kqueue, FSEvents, `clonefile`, `F_FULLFSYNC`, Seatbelt for sandboxes.
  - Simulation: a seeded virtual clock, shuffled completions, injected failures, torn writes,
    power cuts. Every test of the core runs here over many seeds.
- Steps 4-7 of `docs/driver.md` (workspace load, codegen overlapping store writes, link and
  tests, inputs committed while work runs) are part of v2.


## 4. Scheduler and memory

- **One worker thread per core**, work stealing, steal order by topology: same CCD (EPYC) or
  cluster (Graviton, Apple P/E) first, then the same socket, then any.
- **The task is a query execution**, found dynamically. A task that needs another query's
  result reuses it, waits on it if it's running, or runs it inline.
- **Priority**: interactive requests over background CI; within a request, the critical path
  first (signatures and anything many tasks wait on before leaf codegen). Each request gets a
  fair share of the cores.
- **Memory-budget admission**: every task runs in its own bump arena, so its memory is
  measured; the scheduler keeps an estimate per query kind (learned, persisted) and admits work
  only while the estimates fit the budget (the VM's memory minus a margin).   A task over its
  estimate is let finish; the estimate grows. Never OOM, never a silent slowdown: a request that
  can't fit at all is refused with what it needed.
- **Hard backstop**, because estimates can be wrong: every sandboxed process has a memory limit
  (cgroup on Linux, a resource limit on macOS); the daemon caps its own arenas and spills cold
  results to disk before it reaches its limit; a task that would pass the cap waits.
- **No lock on the hot path**: sharded interners and memo tables, results immutable once
  written, readers never lock.

## Issues this lane takes in

- **#1301 owner thread with pump-wait**: the rule for any wait inside a step: a waiting
  thread serves requests, and a nested request that needs what an outer one holds is cancelled
  and reposted, so no deadlock by construction. (`v2-core`)

## One memo, the database's

The plane schedules; it does not remember. The only memo of results is the query database's
(`docs/v2/plan/lanes/db.md`), keyed by input hash. Before the plane runs a node with a key, it asks the
database through one trait (`Memo::settled(key) -> Option<Outcome>`), and single flight joins
nodes of the same key while one runs. `plane.rs`'s own memo of settled keys goes.

## Windows

A driver for Windows: IOCP for files, sockets and timers, `ReadDirectoryChangesW` for file
changes, job objects for children, alongside io_uring and kqueue. Its sandbox is usercode's
(AppContainer and job objects, #1390).

A child's end is heard from a thread-pool wait on the process (`RegisterWaitForSingleObject`),
which posts it to the port; the job's messages, which Windows does not promise to deliver, only
say that the job went past its memory, and the driver then kills it. A memory-limited job's peak
is at most its limit: Windows refuses any commit past it.

## Done when

- every driver (io_uring, kqueue, IOCP, simulation) implements every Effect
- the worker pool steals within a CCD or cluster first and pins threads
- the plane admits by memory with the hard backstop and holds no memo
- simulation tests over many seeds for every driver path
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
