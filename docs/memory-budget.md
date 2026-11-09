---
status: plan
summary: Memory as a scheduled budget: blaze reads the machine's real limit, admits work by its measured peak, sheds caches under pressure and stops with an error naming what cannot fit, never swapping a machine to death.
date: 2026-11-05
---
# Memory as a scheduled budget

Where this leads: [memory-pool.md](memory-pool.md), memory as one buffer pool with one evictor.

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
units than cores run at once. A unit whose prediction fits the budget at all starts, alone,
once no unit granted memory runs, whatever the process holds besides: waiting could never make
room for it, so it is never refused for that. While a unit waits for memory the process holds
besides its grants (`Plane::short`), the daemon lets go of the kept front ends of other
checkouts and asks each build's front end for a release, before the unit starts. A unit's phase
with no record of its own is predicted from its size, its source's bytes times what its kind
held per byte lately, never the largest peak of its kind (memadmit: one crate's 15.78 GB
codegen was every codegen's prediction). Workers
ask for a grant on the socket they already have with the daemon (one CBOR message each way,
rule no-mcp) before a program's build, a link or a test program starts, and give it back when
it ends with the peak they measured.

### 3. Caches with a bound

salsa's `lru` only evicts when a revision begins, and cannot be put on `infer` or the MIR
queries, which have cycle handling (research/daemon-memory.md).
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

### 7. The ceiling: blaze never takes the machine down

2026-11-05: a 128 GiB Mac kernel-panicked twice (watchdog timeout, the memory compressor at
"100% of segments limit (BAD)") with five daemons resident at 98.8, 76.7, 70, 42 and 18.6 GiB.
Each daemon had its own budget and none knew of the others; one ran at 117.66 GiB over its own
81.21 GiB limit because admission granted every cold unit 64 MiB (#1726). So admission alone is
no bound. Four parts, each a bound on its own:

**The machine budget**, one for every daemon of the user on the host. It is the least of the
machine's limits (section 1: `hw.memsize`, `MemTotal`, cgroup `memory.max`/`memory.high`) times
3/4, less what is not blaze's at start and the margin. `BLAZE_MACHINE_MEMORY_GB` sets it.
`BLAZE_DAEMON_MEMORY_GB` (`--memory-gb`) is one daemon's ceiling, never more than the machine
budget.

**The ledger**, `/tmp/blaze-<uid>/machine` (never under `BLAZE_HOME`: two homes are one
machine), a small file of fixed records read and rewritten under `flock`. One record a daemon:
its pid and process start time (so a reused pid is not mistaken for it), home, version,
ceiling, and its charge, `max(held, granted)`, rewritten as its plane takes a `Held` (at most
every 250 ms, or at once when it moves by more than 1/64 of the budget). A daemon joins at start
and leaves at exit. A record whose process is gone, or whose start time differs, is pruned by
whoever takes the lock next. A daemon's **allowance** is the least of its ceiling and the
machine budget less every other live daemon's charge. That allowance, not a fixed number, is the
budget its plane admits against and its releases are decided on (`admission::set_budgets`,
`Squeeze`). The arithmetic is a sans-IO core (`blaze_graph::ledger`); the flock and the file
are its driver (`crates/blaze-cargo/src/serve/machine.rs`). A daemon that cannot open or lock the
ledger stops with an error naming the file (no fallback to a budget of its own).

As built (2026-11-05), where it differs from the above and why:
- The core is `blaze_graph::ledger`, not `blaze_graph::machine`: that name is the build graph's.
- Every ledger write is one turn under the lock: read, prune, compute the allowance, give it to
  the plane (`PlaneInput::Allowance`), write the charge `max(held, granted)`, unlock. Between
  turns the plane grants only up to its **room**, `min(allowance, max(granted, held))` as of the
  last turn, so what it grants never passes the charge the others can read. A plane that has
  never been given an allowance has no room limit, so the plane alone behaves as before.
- Each record keeps the machine budget its daemon brought, and the budget is the least over the
  live records. Daemons started at different times see different "not blaze's" memory, and
  taking the least keeps the bound one number for all of them. At join, the other daemons'
  held bytes are counted as blaze's, not as "not blaze's" (Linux only: macOS has no in-use
  reading yet, so its budget is 3/4 of `hw.memsize` less the margin).
- A task predicted at the whole ceiling (one with no record, which runs alone) starts alone
  with its grant clamped to the allowance once the allowance is above zero. Without the clamp
  it would wait forever for a machine the other daemons share.
- Besides every 250 ms and moves past 1/64 of the budget, a daemon also turns 25 ms after its
  plane held a task back for memory, so freed room reaches it quickly.
- The file is fixed size (a 32-byte header, then 64 records of 128 bytes), checksummed, rewritten
  whole and never synced: it describes live processes, so after a reboot nothing in it matters.
  A file that fails to decode is an error naming it; it is removed by hand, never reset.
- Start time is the kernel's own: clock ticks since boot on Linux (`/proc/<pid>/stat`), and
  microseconds since the epoch on macOS (`proc_pidinfo`). It is only compared with itself.
- Windows has no ledger yet, and neither does the seed without `native`: each keeps its own
  ceiling as before.

**The ceiling, enforced past admission.**
- Soft, at the allowance: the plane admits nothing new, the daemon releases its front ends'
  caches and gives back its heaps' free pages (section 3), and every client with a build held
  back is told (below).
- Hard, at the daemon's ceiling: the kernel's own limit.
  - On macOS, `memorystatus_control(MEMORYSTATUS_CMD_SET_MEMLIMIT_PROPERTIES)` on the daemon's
    own pid, with a fatal active and inactive limit. This is private (rule
    macos-private-apis): the kernel kills the daemon instead of the machine paging itself to
    death, and a test checks the symbol is there. XNU allows it only to root or to a process
    with the `com.apple.private.memorystatus` entitlement, even on its own pid
    (`kern_memorystatus.c`). A daemon run by a user gets `EPERM`, says so once in its log, and
    the watch below is its bound.
  - On Linux, `memory.max` of the daemon's own cgroup where one is delegated
    (`blaze_core::cgroup`), `memory.high` at 15/16 of it.
  - Where neither applies, the daemon's watch thread (100 ms, the footprint
    `resident_by` reads) ends the daemon with exit status `MEMORY_CEILING` once its footprint
    passes the ceiling, after a postmortem naming the tags and the running tasks.

  In each case the client says the daemon was stopped at its ceiling, naming it and the
  ceiling, as a typed failure (`the daemon (pid N) was stopped at its memory ceiling of 48
  GiB`), never a bare hang-up.

**Pressure.** The system says before a budget can (`blaze_core::Effect::Pressure`, one more
event of the driver's own wait, no thread):
- macOS: a kqueue knote of `EVFILT_MEMORYSTATUS` (warn, critical), the private filter
  `DISPATCH_SOURCE_TYPE_MEMORYPRESSURE` is made of; the public `EVFILT_VM` is `bad_filtops` in
  XNU now.
- Linux: a PSI trigger, `some 300000 2000000` (15% of a 2 s window: a process without
  `CAP_SYS_RESOURCE` may arm only whole multiples of 2 s, Linux 6.5+), on the daemon's cgroup's
  `memory.pressure` where the memory controller is delegated to it, else on
  `/proc/pressure/memory`, polled through the ring for `POLLPRI`.
- Where there is none (no PSI, a kernel before 6.5, Windows for now), the daemon says so once in
  its log and admits by its budget alone.

Either event makes the daemon act as at the soft ceiling now: no admission, release, give
back. It keeps its allowance at what it holds until 10 s pass with no event
(`blaze_graph::pressure::Pressured`, a sans-IO core). The plane takes it as
`PlaneInput::Cap`; its budget is `min(allowance, cap)` (`blaze_graph::pressure::budget_under`,
`Plane::budget_now`, the one join), every admission decision reads that, and under a cap no
task starts alone (waiting does make room there). A task is refused as too big only against the
allowance, never the cap. The front ends are asked for a release at the first event of a run
and once each 10 s of it after.

**What the client is told.** A build held back for memory for more than a second (a task of it
ready, a thread of its pool free, and no room: `Plane::waits_for_memory`) is told, as a note
(`Response::Diagnostic` of `Diagnostic::note`, no new protocol), once, then again when what the
daemon holds moves by more than a 64th of the budget or the tasks it waits on change, and
`build resumed` once none of it is held back (`blaze_graph::pressure::Paused`):
`build paused: daemon at 47.9 of 48 GiB (machine 96 GiB, 40.1 held by pid 4242 of
/Users/a/.blaze), waiting on 3 tasks: codegen of hir-ty, ...`. It names what waits, on what,
and who holds the rest.

**Proof.** A Hegel DST of the ledger core and the plane's admission against it
(`crates/blaze-graph/src/ledger/tests.rs`).
- The simulation runs several daemons, each with tasks whose grants and real use are drawn.
  Their steps interleave with crashes (a daemon gone without leaving), pid reuse and restarts.
- The oracle:
  - at every step the live daemons' grants sum to at most the machine budget;
  - no daemon admits while the charges sum to it;
  - a task that fits is admitted within a bounded number of steps after others end
    (bounded-progress).
- Planted-bug controls it must catch:
  - a read-modify-write of the ledger without the lock (two daemons both admit into the same
    room);
  - a prune that ignores start time (a dead daemon's charge kept by a reused pid);
  - an allowance that leaves out another daemon's charge.

### 8. One daemon per machine

Two daemons on one machine are two of everything: two front ends of the same std, two stores'
working sets, two budgets. By default blaze runs one daemon per user per machine. It holds
`/tmp/blaze-<uid>/daemon.lock` (machine-wide, not under `BLAZE_HOME`), with its socket and its
version in the ledger.
- A client of another `BLAZE_HOME` of the same version uses the running daemon, which serves
  that home's store and builds beside its own (one home a request, as a workspace is today).
- A client whose binary differs (a lane testing a blaze it just built) does not start a second
  daemon. It stops with an error naming the running one: its pid, home, version and resident
  memory, and how to stop it or opt out.
- `--isolated` (or `BLAZE_ISOLATED=1`) is the opt-out. It starts a second daemon that joins the
  ledger like any other, so its charge counts against the one machine budget.

Never two independent ceilings: whatever runs, the ledger's budget is the bound. The ledger and
the budget land first (section 7). The single daemon and its reuse across homes come next, once
a daemon can serve more than one home's store.

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
9. The ceiling (section 7): the ledger and machine budget, the kernel's limit, pressure,
   what the client is told; its DST.
10. One daemon per machine (section 8).

Each step is its own pull request with its numbers: the peak and time of the tests job before
and after.
