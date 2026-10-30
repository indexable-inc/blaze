---
status: plan
summary: Memory as a scheduled budget: blaze reads the machine's real limit, admits work by its measured peak, sheds caches under pressure and stops with an error naming what cannot fit, never swapping a machine to death.
date: 2026-10-29
---
# Memory as a scheduled budget

blaze must never take a machine down. When what it was asked to build does not fit in the
memory it may use, it stops with an error that names what did not fit and how much it needed
(AGENTS.md rule no-fallback), rather than swapping until the runner is lost or the kernel kills
something. This note is the design and the order it lands in.

## Why: the tests job, 2026-10-29

The `linux x64 / tests` job (hosted `ubuntu-24.04`: 4 cores, 15.6 GB, and 20 GB of swap that
`.github/actions/blaze` adds) has not finished on main or on a pull request since the
bootstrap blaze went from 0.20261028.5 to 0.20261029.1 (#1487). The last green run built in
1744 s. Since then each run either:

- is lost with "The runner has received a shutdown signal" (exit 143) about 25 minutes in,
  after about 15 test programs started linking within half a second and each link took minutes
  (main run 40916953450; pull requests' 40918204059 and 40924026897), or
- stops making progress and runs into the step's limit (150 or 335 minutes).

[#1503](https://github.com/indexable-inc/blaze/pull/1503) made `harness/unittests.py` say every
30 s what the machine, the run and its largest processes hold. Its first run (job 123640427488)
was of the second kind: from minute 3 to minute 150 the worker held 8.2 GB, the machine 9.2 GB
and swap nothing, and one program of the 21 linked. So that run was stuck, not out of memory.
The runs that were lost had, before they were lost, the shape of too much at once: every test
program built side by side and linked at the same time. 0.20261029.1 brought both changes that
add work at once: #1428 asks a cold worker for every program of the checkout together (the
worker builds every stale program on a thread of its own, unbounded,
`crates/blaze-engine/src/lib.rs`, "The programs asked for are built side by side"), and #1459
compiles each wave of a test program's reach on every core, for every program at once. The
bisect of the two is [the cause](#the-cause) below.

Nothing in blaze bounds any of this by memory. A worker builds as many programs as it is asked
for, each fans out on every core, each starts `cc` and lld when its code is done, and the
tests then run as they come. Only two things look at memory now: the daemon stops idle workers
past `BLAZE_MEMORY_MAX` (half the machine), and the own base that nobody waits for stops past
`BLAZE_OWN_BASE_MEMORY` (a quarter). Neither keeps a build from taking more than the machine has.

## The cause

Two, one hiding the other.

**A deadlock in 0.20261029.1.** gdb's stacks of the stuck runs (#1503, job 123773519177): the
worker's main thread waits in `Builds::settle` for salsa's `cancel_others`, that is for every
other handle on the database to be dropped, while four program builds sit in
`ObjectCodegen::compile_ahead`'s thread scope (`in_parallel_beside`) with sixteen threads parked
on its condition variable, each holding a handle and none looking at the cancellation.
[#1482](https://github.com/indexable-inc/blaze/pull/1482) ("compiling ahead ends when the build
is cancelled") fixed this on main, in 0.20261029.3 and later; the bootstrap moves to
0.20261029.7 in [#1538](https://github.com/indexable-inc/blaze/pull/1538).

**Programs built side by side without a bound.** With the deadlock out of the way the bisect
probes measure what is left:

| bootstrap | has | the worker at most | the run |
| --- | --- | --- | --- |
| 0.20261028.14 ([#1512](https://github.com/indexable-inc/blaze/pull/1512)) | neither | about 14 GB resident and 5 GB swapped | still linking test programs at 55 min |
| 0.20261029.0 ([#1511](https://github.com/indexable-inc/blaze/pull/1511)) | #1428 | 14.6 GB resident and 21.8 GB swapped | swap full at 28 min, runner lost (exit 143) |

#1428 asks a cold worker for all of the checkout's programs at once, and the worker builds each
on a thread of its own: twenty-one programs' codegen at once, about 36 GB on a 15.6 GB machine.
Even one at a time the worker reaches about 19 GB, because every program's memos stay in the
database for the next ([section 3](#3-caches-with-a-bound)): admission alone brings the tests
job back to where it was on 2026-10-28, and only shedding caches brings it under the machine.

## The design

One budget per machine, owned by the daemon. Every piece of work that can hold much memory is a
**unit**: it has a name a person can read, an identity that is the same from one build to the
next, and a predicted peak. A unit starts only when the daemon grants it room. The daemon
watches what is really held and grants nothing more when it is close to the budget.

### 1. The limit

`memory::of_the_machine()` (`crates/blaze-cargo/src/memory.rs`) already reads the fewer of
`MemTotal` and the cgroup's `memory.max` (version 2) or `memory.limit_in_bytes` (version 1) up
the process's groups; on macOS, `hw.memsize`. The budget is that, with two changes:

- cgroup v2's `memory.high` counts as a limit too: past it the kernel throttles and reclaims,
  which is the slow death this note is about.
- The budget is the limit less what is not blaze's when the daemon starts (`MemTotal -
  MemAvailable`, outside the daemon's tree), less a reserve of 5% (at least 512 MB) for the
  kernel and the page cache that the store's reads need.

Swap is not budget: a build that needs swap to finish is a build that does not fit.
`BLAZE_MEMORY_BUDGET` sets the limit (a size, or `off`, the way out for a person who says so),
for everything blaze runs; `BLAZE_MEMORY_MAX`, the share the daemon's idle workers may keep, is
then a share of the budget. The daemon says the budget and where it came from at start
(`memory budget:` in its log, built) and in `blaze status` (to come).

### 2. Units, their peaks, and admission

The units, from the largest down:

| unit | runs in | its peak, measured as |
| --- | --- | --- |
| a program's build (its codegen and objects) | the worker | the rise of the worker's high-water mark (`VmHWM`, reset through `/proc/self/clear_refs` with `5`) while it alone runs; with others beside it, its share of the live heap (`memory::live`) at its end |
| a link (`cc`, lld) | a child process | `wait4`'s `ru_maxrss` |
| a build script, a proc-macro server | a sandboxed child | `wait4`'s `ru_maxrss` |
| a test program | a child process | `wait4`'s `ru_maxrss` |
| a workspace's load | a new worker | the worker's `VmHWM` when it is ready |

Each measured peak goes into the store as a small record keyed by the unit's identity without
what changes from edit to edit: the package, the target, the profile and the kind of unit (the
program's plan key, a link's output name, a build script's package and version). The
prediction for a unit is its last record, raised by 20% so that an edit's growth fits; a unit
with no record is predicted at the whole free budget, so that it runs alone the first time.
Records travel with the remote cache like the rest of the store, so a CI runner's first build
already knows its units.

The daemon keeps the ledger: `granted`, the sum of the predictions of the units running, and
`held`, what its tree holds now. A unit is admitted when `max(granted, held) + prediction <=
budget`. Units that wait start largest first among those that fit, and no more CPU-bound
units than cores run at once. A unit whose prediction is the whole budget runs alone. Workers
ask for a grant on the socket they already have with the daemon (one CBOR message each way,
rule no-mcp) before a program's build, a link or a test program starts, and give it back when
it ends with the peak they measured.

### 3. Caches with a bound

salsa's `lru` only evicts when a revision begins, and cannot be put on `infer` or the MIR
queries, which have cycle handling ([research/daemon-memory.md](research/daemon-memory.md)).
So a build holds every memo it computed until it ends. Under pressure:

- Machine code and objects already written to the store are dropped from the worker and read
  back by map when needed (file-backed pages the kernel can take back), as the own base's MIR
  and code already are (#261).
- Between waves of a program's build (`tests_reach`'s waves, the codegen units of `units.rs`)
  the worker may begin a revision that changes no input, which lets `lru` evict, and calls
  `memory::give_back` so the allocator returns the pages.
- The memos of functions whose code is sealed in the store are evicted under a byte bound per
  query, counted by the size the store records.

### 4. Codegen that streams

A program's objects are built whole in memory (`write_object` returns a `Vec<u8>`) and kept
until its link. They are to be written to their file as each codegen unit is done, and dropped,
so that a program's peak is its largest unit's and not the sum of its units; the link reads
them from disk. lld's peak is measured as a unit's own (above), and its threads are those of
its grant.

### 5. The live watch

A thread of the daemon reads every 100 ms what its tree holds (from its own cgroup's
`memory.current` where blaze runs in one, else `/proc/PID/statm` of each process of the tree,
on macOS the footprint). At 85% of the budget it grants nothing new. At 95% it asks each worker
to shed (section 3). Work under way is never paused in the middle of a unit; what pauses is
the admission of the next one.

### 6. A unit that cannot fit

When one unit's prediction from a record is more than the whole budget, blaze stops before it
starts, with an error that names it, its measured peak, when it was measured, and the budget
and where the budget came from:

```text
error: the test program of `hir-ty` (release) needs 9.4 GB, measured on 2026-10-29, and blaze
  may use 7.1 GB here (cgroup memory.max of /system.slice/runner.service, less 0.8 GB in use
  by other processes): give the machine more memory or set BLAZE_MEMORY_MAX
```

A unit with no record that grows past the budget while it runs alone is stopped, its peak so
far recorded, and the build ends with the same error. Never a retry with less parallelism
behind the user's back, never swap.

## The check in CI

`harness/unittests.py` says the most the machine held, memory and swap, in the job's summary,
and fails the job when it is over a bound the workflow passes in its configuration, set from
the measured peak of the tests job with room for growth. A harness runs a build of blaze's own
workspace under a cgroup with a `memory.max` below that peak (`systemd-run --user --scope -p
MemoryMax=`), and checks that blaze either finishes within it or stops with the error of
section 6, never killed by the kernel and never into swap.

## The order it lands in

1. The watch and the bound in CI (#1503).
2. The limit (section 1): one budget, `memory.high`, what others hold, said at start (#1516).
3. The cause of 2026-10-29 under a first bound: the bootstrap with #1482 (#1538), and the
   programs a worker builds side by side admitted by the budget
   (`crates/blaze-engine/src/admit.rs`; #1539 added it without calling it, the worker calls it
   since the pull request after, which names each program by the object its client asked
   for): a program not yet measured in the worker runs alone,
   and its peak, the most more than at its start the worker held while it ran (read every
   100 ms), is its prediction afterwards. Measured in the worker's memory only, not yet in the
   store, so a new worker runs each program alone once.
4. Measuring units and recording their peaks in the store (section 2's table).
5. The ledger and grants in the daemon, for program builds, links, test programs, build scripts.
6. The live watch, and the error of section 6.
7. Caches under pressure (section 3).
8. Streaming codegen (section 4).

Each step is its own pull request with its numbers: the peak and time of the tests job before
and after.
