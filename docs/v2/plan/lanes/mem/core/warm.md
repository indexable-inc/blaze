---
status: plan
summary: mem-core-warm - the IOCP driver hands each sandboxed spawn to the confine's one warm launcher (blaze sandbox --v2-serve) as a frame and reads its answer, never confine() or a cold start; per-run state in the spawn slab tagged Io, a reused answers buffer and frame queue, nothing allocated in the driver beyond the frame; each step's cost against its bound.
date: 2026-10-29
---

# mem-core-warm: the warm launcher on Windows, against its bound

Child of [mem-core](../core.md) ("The warm launcher on Windows" there says what it does).
Paths: `crates/blaze-core/src/iocp.rs`, `crates/blaze-core/src/iocp/{warm,rundirs,process,sys}.rs`.
Nothing was built or run; the tests are written beside the code for the coordinator's run.

## Speed of light

The floor for a sandboxed run is the launcher's one `CreateProcessW` (in its job, under its
token), on the order of 5 to 15 ms on Windows; a cold start (a new `blaze sandbox` per run) adds
another process creation and the sandbox's setup, ~20 to 50 ms more. The driver's share is meant
to stay well under the floor.

| step | bound | where it lands |
|---|---|---|
| a handed spawn, in the driver | `CreateJobObjectW` + 2 `SetInformationJobObject`, 3 pipes (`CreateNamedPipeW` + `CreateFileW` each), 4 `DuplicateHandle` into the launcher, `CreateDirectoryW`, 3 port associations, 1 job association with the port, the frame's `WriteFile`: ~20 syscalls, ~100 to 200 µs; 0 allocations | the same: pipe names on the stack, the run directory's path reused, the run map and frame queue reserved; the one allocation is the frame `Confine::hand` makes (and the spawn's own `Box<Exit>` and output, as for any spawn) |
| an answer | 12 bytes cut from the reused answers buffer, one `runs` lookup, the spawn's finish: no syscall beyond the read that brought it | the same; a split answer is carried in 12 bytes, never a buffer |
| the run's end | the finish, its code the answer's (no process to ask), `DeleteFileW`/`RemoveDirectoryW` per entry of the run directory through one reused wide buffer | the same |
| the launcher's start | once a daemon (and again only after it ends): 2 pipes, 1 `DuplicateHandle` of stderr, 1 `CreateProcessW`, 1 `RegisterWaitForSingleObject` | the same; a launcher that has exited is told by a 0 ms wait before a run is handed |
| a kill | `QueryInformationJobObject` + `SetInformationJobObject` (seal) + `TerminateJobObject` | the same |

## Open

- The launcher is started in no job and without `CREATE_BREAKAWAY_FROM_JOB`; where the daemon
  itself sits in a job that forbids breakaway, the launcher stays in it, as the daemon's other
  children do.
- Frames are written one at a time; a burst of runs waits on each write's completion (one pipe
  buffer's worth each, far under the floor), not on the launcher.
