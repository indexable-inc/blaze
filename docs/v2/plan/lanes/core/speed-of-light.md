---
status: plan
summary: lane core's speed of light: the io_uring, kqueue, IOCP and threads drivers, the plane, the executor and pool, and the watchers; the work each does counted per effect, per task and per change batch, the bound the hardware and the work set, what today's code does against it, and how the design reaches 90% of it.
date: 2026-10-30
---
# Lane core: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here; the counts are
read from the code (nothing was built or run for this pass), the bounds are arithmetic on the
figures below. Speed's earlier notes say what was changed before this pass:
[speed-uring.md](speed-uring.md), [speed-runtime.md](speed-runtime.md),
[speed-runtime-plane.md](speed-runtime-plane.md), [speed-runtime-pool.md](speed-runtime-pool.md)
and [plane.md](plane.md).

Sections and who wrote them: the io_uring driver and the threads driver, and the Linux watchers
(speed-sol-core-uring); the kqueue and IOCP drivers and the macOS and Windows watchers
(speed-sol-core-ports); the plane, the executor and pool (speed-sol-core). Each edits only its
own sections.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe read | about 12 GB/s (PCIe 5 x4) |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

Added for the core, from the same sources' kind of figure:

| what | figure |
|---|---|
| a cache miss to DRAM | about 90 ns (Turin, local node) |
| a cache line moved between cores of one CCD | about 30 ns; across CCDs about 100 ns |
| an uncontended atomic RMW on a line the core holds | about 20 cycles (6 ns) |
| a futex wake of a sleeping thread, to it running | about 5 µs (wake syscall 1 µs, plus the scheduler's switch-in) |
| `io_uring_enter` with N SQEs | about 0.15 µs plus the work of each SQE |
| a 4 KiB page-cache read through io_uring | about 1 µs of kernel work (lookup, copy at 10 GB/s, CQE) |
| `statx` of a cached dentry | about 0.4 µs |

## The reference workloads

- **A wide build**: 100,000 tasks of 50 µs each on 64 cores, a DAG with a critical path of 200
  tasks (10 ms). Work is 5 s, so the bound is max(5 s ÷ 64, 10 ms) = 78 ms. The scheduler's
  budget is 10% of it: 7.8 ms, or 5 µs of a core per task (7.8 ms × 64 ÷ 100,000).
- **A warm build of cache hits**: 1,000,000 tasks of 2 µs each (each a database lookup that
  hits) on 64 cores: work 2 s, bound 31 ms, the scheduler's budget 3.1 ms, or 0.2 µs of a core
  per task (3.1 ms × 64 ÷ 1,000,000), about 700 cycles.
- **A cold workspace load**: 50,000 source files of 20 KiB average (1 GB) stamped, opened and
  read; 5,000 store writes of 200 KiB (1 GB) written whole (temporary, sync, rename).
- **An edit**: one file saved: one change batch from the watcher, one stat, one read.

## The plane

`crates/blaze-graph/src/plane.rs` is the deterministic state machine every build goes through:
single flight by key (`flight`), stride fair share between requests, critical-path priority, memory
admission, and the memo asked at `became_ready`. `machine.rs` wraps it for one request.

**The work, per task, from add to finish.** A node is added (its row, its edges), becomes ready
when its last dependency settles, is dispatched (taken off the ready heap, admitted, started), and
finishes (its outcome settles each dependent's count). What must be touched:

- its `NodeRow` (64 B, one line) and `TaskRow` (32 B, half a line);
- its `NodeCold` (64 B) only on add, on a failure and at the end;
- one `Edge` (12 B) per dependency, written on add and read once on settle; the dependents' rows
  (one line each, the row's waiting count);
- its `Ready` entry (24 B) in a 4-ary heap: a push and a pop, each log₄ of the ready set, the top
  lines hot;
- the request's row, hot.

**The bound.** With E edges a node: 1.5 lines of row, plus E × 12 B of edges, plus E dependents'
lines, plus about 3 heap lines (log₄ 100,000 ≈ 8 levels, the top 5 shared and hot), so about
5 + E lines; at E = 3, 8 lines. If the graph is bigger than L2 (100,000 nodes × 96 B = 9.6 MB) and
in L3, a line is about 15 ns, about 120 ns a task. A few hundred instructions (an index probe,
counter updates, two heap operations, the stride pick) is about 150 cycles, 40 ns. The bound is
about 0.15 µs a task, under 10% of a 2 µs cache hit as the task asks. The plane must be no more
than 1.1 × that: 0.17 µs, or 0.2 µs at most, the warm build's whole budget.

**Today.** From reading `add_one`, `became_ready`, `start`, `finish_task`, `settle` and
`dispatch`:

- **Hash probes:** about 6 per node. `index: FxHashMap<NodeId, NodeHandle>` is probed by `check`
  (an entry), by each dependency's lookup (one a edge), by `add_one`'s `get_mut`, by `Done`
  (a get) and at the end (a remove). `request_index` is probed about twice a step, and `flight` an
  entry and a remove for a keyed node. Each probe of a 100,000-entry table is a line out of L2:
  about 6 × 15 ns = 90 ns.
- **Rows and heaps:** the row columns and heaps are as above (heap entries carry `heap_at`
  back-pointers, so a reprioritised entry moves without a search).
- **Records:** about 4 `PlaneOutput::Said` per node (Waiting, Ready, Running, Built), each an
  output of about 144 B (an `Event` is about 128 B). That is 576 B, or 9 lines, written a node and
  read by the owner.
- **Total:** about 30–40 lines a task with records and probes, and about 1,000 instructions,
  roughly 0.4–0.6 µs a task.

So today is about 3× the bound on the plane alone. It is fine for the wide build (0.5 µs of
5 µs), not for the warm one.

**What changed in this pass.** `Machine::step` allocated a fresh `Vec` of outputs every step.
`Machine::step_into` now appends to the caller's buffer, and `Machine::step` is a wrapper over it.
The executor's `drive` uses it, so a warm `Done` through the machine allocates nothing. The tests
are `step_into_gives_what_step_gives` (200 random builds, refusals included, step for step equal
to `step`) and `a_warm_done_through_step_into_allocates_nothing` (blaze-alloc-count, a budget of
zero), in `crates/blaze-graph/src/tests.rs`. The plane's own `said` buffer was already kept across
steps.

**The gap, and how the design reaches it.** Three steps take the plane from about 0.5 µs to about
0.15 µs. None changes what the plane decides, so its determinism holds.

1. **Handles, not ids, after add.** `PlaneOutput::Run` carries only the `NodeId`, so `Done` looks
   the node up again. If `Run` also carried the plane's `NodeHandle` (a row index and a
   generation, 8 B), the executor would keep it in its run slot beside its own table handle, and
   `Done` would give it back. The generation check refuses a stale one exactly as the id lookup
   does now. That removes about 3 of the 6 probes a node, leaving `check`'s entry and one per edge
   at add. It changes `PlaneInput::Done` and `PlaneOutput::Run` across the plane, the machine and
   exec, all in lane core. It waits on a measured profile, not done blind in this pass.
2. **Columnar records.** The 4 records of about 144 B become one 16 B entry each (node handle,
   state, time) in a per-request column. The `Event` is built only where a reader wants it: the
   protocol's subscriber, at its own pace. That cuts 9 lines a node to 1. It changes
   `PlaneOutput::Said` and the executor's `heard` channel, so it lands with that design, below.
3. **Spec without strings.** `Spec` carries a `String` label and a `Vec` of dependencies per node,
   allocated by the caller that adds (not on the owner's path). A columnar add (labels in one
   arena, dependencies as one slice of all the edges) removes 2 allocations a node at add.

**Allocation audit (2026-10-30).** `plane.rs` and `machine.rs` have 51 sites of `clone`,
`to_owned`, `to_vec`, `collect`, `Vec::new`/`with_capacity`, `Box`/`Arc::new`, `String` and
`format!` outside tests. None allocates on a warm step:
- 23 are kept scratch, or columns started empty that allocate nothing until their request
  fills them.
- 12 are the capacity of a batch's columns, once per add.
- 9 are errors.
- 5 are the step's outputs for a caller that asked for its own buffer (`step`, `cancel`, the
  machine's `step`); `step_into` is the path that reuses its buffer.
- 2 are the `NodeState` clone in `moved` and `settle`, which allocates only for `Failed`.

So none of the 51 was hot. The per-node `String` and `Vec` of `Spec` (step 3 above) is the one
allocation left per node, on the caller's thread, and `PlaneInput::AddNodes` is the columnar
path that avoids it. The coordinator's figure of 10 to 13 such sites per 1,000 lines counts these
cold ones.

**What proof's budgets should measure.**

- A 100,000-node DAG of E = 3, driven through `Machine::step_into` with zero-length work:
  instructions and L2 misses per task (`perf stat`-style counters).
  - Budget: ≤ 1,500 instructions and ≤ 40 lines today. After the three steps: ≤ 600 instructions
    and ≤ 12 lines.
- Allocations per warm `Done`: zero (the test above, kept as a budget).
- The plane's ns per task at 1,000,000 nodes, against 0.2 µs.

## The executor and pool

`exec.rs` is the scheduler: one owner thread steps the plane, and the pool (`pool.rs`) runs the
work. `machine.rs` is the single-request driver. `admission.rs` and `arena.rs` are the memory
budget, the throttle per tag and what blaze holds. The daemon's builds go through cargo's loop,
`crates/blaze-cargo/src/serve/plane.rs`, which steps the same plane on the same pool. That loop is
lane cargo's, counted at the end of this section.

**The work, per task.** One message to a worker (the job) and one back (its `Done`); a wake only
when every worker sleeps; the plane's step; the records to whoever asked.

**The bound.** No allocation; one owner message each way; a wake only when a worker sleeps. On the
owner, the plane's 0.15 µs, plus two cross-core lines (the job pushed, the `Done` taken), about
0.1 µs: 0.25 µs a task. The scheduler adds under 10% on top of max(work ÷ cores, critical path)
when two things hold:

- the owner keeps up: per task, at most cores ÷ task length of serial time, less a 10% margin;
- each hop of the critical path adds under 10% of its task.

**Today, per task, on the `exec::Scheduler` owner** (read from `Owner::take`, `step`, `start`,
`post` and `run_pooled`):

| step | count | cost |
|---|---|---|
| the `Done` off the mailbox | 1 `try_recv` of a bounded array channel (`MAILBOX`, 4,096), the line from the worker | about 0.05–0.1 µs |
| the plane's step | as the plane section counts | about 0.5 µs |
| records to the request | about 4 `heard.send` on an unbounded channel, a 31-slot block allocated every 31 | about 0.15 µs |
| the run slot | a `Table` slot, pooled, no allocation | about 0.05 µs |
| `Throttle::admit` | 2 loads of the tag's line (`blaze_mem::tag::over_budget`), which workers write | about 0.05–0.1 µs |
| the job pushed | `push_placed` to a group's `Inject` (a lock, an intrusive link, no allocation); `announce` wakes a sleeper only if none is searching | about 0.1 µs, plus a 1 µs futex wake when every worker of the group sleeps |
| what blaze holds | was `arena::own()` after every `Done`: `held()` plus `blaze_mem::tag::snapshot()`, 100 loads over 20 lines workers write | was about 2 µs |

The pool's own counts per task:

- **Messages:** 1 push and 1 pop of the job, and 1 `Msg::Done`.
- **Steals:** 0 in a saturated build. The owner isn't a worker, so every job goes to an
  `Inject` and every worker takes from one; a job given way is posted again near the group it ran
  on.
- **Allocations:** none for the job, since the run slot is the intrusive job (`Job::from_header`)
  and pool job cells are cached per thread.
- **Wakes:** none in a saturated build. The owner parks on the mailbox only when it is empty; then
  the next `Done`'s send wakes it, about 1 µs of the worker's time.

**What changed in this pass** (all in `crates/blaze-graph/src/exec.rs` and `machine.rs`):

- **What blaze holds, once per batch.** The owner now takes up to `HELD_EVERY` (64) messages that
  are already waiting, then reads `arena::own()` once, and only if a `Done` came and the plane has
  a budget. It always reads before it waits, so the plane never decides on a reading older than
  the last batch. That is 2 µs ÷ 64 ≈ 0.03 µs a task, from 2 µs. Without a budget it is never
  read, as the plane ignores it.
  - Queue 00863 (lane mem) makes the reading one load: `blaze_mem::tag::committed_now()`, with
    `arena::own()` using it.
  - The plane hears the same holdings at more widely spaced times. With a budget, a run already
    admitted against a reading 64 messages old is at most 64 tasks' growth behind, the same slack
    cargo's loop has (it reads once per poll).
- **`Machine::step_into`**, as the plane section says. `drive` uses it with one kept buffer.
- **`drive`'s `done` channel** is bounded at `work + io`: at most that many run at once, each
  sends once, so it never fills. It allocated a block every 31 sends.
- **`Msg::Add`** takes the `works` entry once (it was a `contains_key` and then an `insert`).

**Per task now: about 0.8 µs on the owner** (about 2.8 µs before).

**The wide build.** 100,000 tasks of 50 µs on 64 cores need the owner at 64 ÷ 50 µs = 1.28 M
tasks a second, 0.78 µs a task.

- **Before:** at 2.8 µs the owner was the bottleneck: 100,000 × 2.8 µs = 280 ms against the 78 ms
  bound, 3.6×.
- **Now:** 0.8 µs gives 80 ms, within 10% of 78 ms (the budget is 86 ms), but with the owner
  near 100% busy, so queueing at the mailbox can take the margin.
- **Critical path:** 200 hops × (about 1 µs of owner latency, plus a 5 µs wake if the next worker
  slept) ≈ 1.2 ms on 10 ms, inside the budget.
- **With margin:** that needs the owner at 0.5 µs a task. That is the plane's handles and
  columnar records (the plane section), plus records sent per step, not per record: one `heard`
  send of the step's records, which takes about 0.1 µs off.

**The warm build cannot be reached by a single owner.** It is 1,000,000 cache hits of 2 µs on 64
cores, a 31 ms bound. Keeping up needs 64 ÷ 2 µs = 32 M tasks a second, about 31 ns of serial time
a task, and staying within 10% of the bound leaves 34 ms of serial owner time in all. Today's
0.8 µs is 800 ms, 26×. Even the plane at its own bound (0.15 µs) is 150 ms, 5×. The serial step
per task is what stands in the way. Two designs remove it:

1. **Settle on the workers.** Each node row keeps an atomic count of what it waits on. A worker
   that finishes a node decrements its dependents' counts (E relaxed RMWs, about 0.1 µs, in
   parallel) and pushes a node that became ready onto its own deque's LIFO slot, as TBB and
   Taskflow do. The owner keeps only the slow path:
   - single flight by key;
   - admission when a budget is near;
   - fair share when more than one request is open;
   - failure;
   - the records, read from a column, not sent.

   The plane's contract (the same inputs in the same order give the same outputs) holds for the
   slow path. For the fast path, the sim tests must show every start order the workers produce is
   one the serial plane gives for some order of `Done`s, which is already a timing order today.
   This is lane core's to build, with differential sim tests and the wide and warm benchmarks. It
   is not done in this pass, which ran nothing.
2. **Prune from the top.** A warm build in which nothing changed should not run a million cache
   hits. The query layer's red-green check from the roots verifies only what changed and what
   depends on it, so the bound becomes O(changed). That is the database and query lanes' design
   (`crates/blaze-query/`, not lane core's), and it is the real fix for the warm case. With it,
   design 1 is needed only for wide cold builds of short tasks.

The `Inject` lock is the second limit at 32 M tasks a second: about 4–8 group locks, each line
moving every 30–100 ns, saturate near 10–20 M a second. Design 1 removes it too, since ready
nodes go to the finisher's own deque.

**Cargo's loop** (`serve/plane.rs`, the path builds take today):

- It already batches completions per poll and reads `arena::held()` once a poll.
- Per task it clones the batch's `Arc<W>` twice and the `done` `Sender` once, and the workers
  drop them: six RMWs on two lines every task shares, about 0.6 µs a task. That is 3× the warm
  budget alone.
- Its `done` and `io` channels are unbounded.

Queue 00864 (lane cargo) asks for one context made once (as `exec.rs`'s `Ctx`), passed by
reference, and bounded channels, with an allocation budget test.

**What proof's budgets should measure.**

- **Wide build:** the benchmark of 100,000 tasks of 50 µs on 64 cores (a synthetic runner that
  spins).
  - wall time ≤ 1.1 × max(work ÷ cores, critical path);
  - owner busy time ≤ 0.8 µs a task now and ≤ 0.5 µs after the plane's steps;
  - readings of what blaze holds ≤ messages ÷ 64 + 1 a batch;
  - futex wakes ≤ 1 a park.
- **Warm build:** 1,000,000 tasks of 2 µs.
  - Today's figure is about 0.8 s against 31 ms; budget the measured figure, so a regression
    shows.
  - After design 1: ≤ 34 ms.
- **Allocations:** zero per warm task on the owner and the workers, the records' channel
  included once it batches (blaze-alloc-count, as
  `a_warm_done_through_step_into_allocates_nothing`).
- **Cargo's loop:** the same two benchmarks through `serve/plane.rs`, which is what users run.

**Allocation audit (2026-10-30).** `exec.rs`, `pool.rs`, `pump.rs`, `admission.rs`, `arena.rs`,
`topology.rs`, `build.rs` and `trace.rs` have 118 sites outside tests:
- 56 are errors.
- About 40 are setup: topology, the budget and threads, once per process or scheduler.
- 12 are kept buffers.
- The rest are per-message payloads.

Three kinds run per task or per message:
- **The pool's `Thread` clone in `Left::drop`, and the pump's waiter `Thread` and value clones.**
  These are reference counts handed to the thread being woken, which is the one case where an
  `Arc` clone belongs. They stay.
- **`Msg::Add`'s two vectors.** The id list of the nodes whose work is new is now kept on the
  owner and reused across adds (`Owner::new`). The `Vec<Spec>` is the plane's input and is owned
  by it. Its per-node strings go with the columnar add (step 3 of the plane).
- **`Handle::found`'s spec and work vectors.** These are the payload, crossing from the worker to
  the owner.

The pool's jobs are slab cells, not boxes. The warm task budget
(`a warm run and its admission`, zero) holds.

## The io_uring driver

`crates/blaze-core/src/uring.rs` and `uring/` run every effect on Linux. There is one ring with
SQPOLL and SINGLE_ISSUER. Each step does one tail store (`Ring::publish`) and each pass one head
store (`consume`). The table of registered files is sparse: half of it is the driver's, half the
kernel's for multishot accepts. Requests live in slab columns `Ops`: a 32 B `Hot`, an 88 B `Cold`
and a buffer column. Names go into two `Paths` arenas, and each `user_data` carries a generation.
fix-maps already did its part before this pass. Timers are a `Wheel<Tag>` instead of one
`OP_TIMEOUT` each. The tag-keyed tables (`files`, `watches`, `listens`, `spawns`, `conns`) are
`FxHashMap`s, not `BTreeMap`s, and a ring failure sorts its spawns by tag. Those parts are not
counted again here.

**The work, for the cold workspace load.** Each of the 50,000 source files is stamped, then
opened, sized, read and closed. Each of the 5,000 store writes is a temporary file opened,
written, synced, renamed over its name, and its directory synced.

**The bound.**

- **Stamps:** 50,000 `statx` of cached dentries at 0.4 µs is 20 ms of one core. Spread over 16
  cores, that is 1.25 ms.
- **Reads, warm page cache:** a file costs an open that resolves a path (0.4 µs), a size
  (0.15 µs), a 20 KiB copy (2 µs at 10 GB/s, plus about 1 µs for the read and its CQE) and a
  close (0.15 µs). That is about 3.7 µs a file and 185 ms of one core. Spread over 16 cores it is
  11.6 ms.
- **Reads, cold:** reading 1 GB from NVMe at 12 GB/s takes 83 ms.
- **Whole writes:** writing 1 GB to NVMe at 7 GB/s takes 143 ms. The CPU side is a 200 KiB copy
  (20 µs) plus about 6 calls a write, about 115 ms of one core. The 10,000 syncs (each file's and
  its directory's) cost latency, not bandwidth, as long as enough are in flight for the file
  system's journal to commit them in groups.

So the cold load is bound by the device at about 143–226 ms (writes, then reads if they do not
overlap). With the sources in the page cache, the reads' share is 12 ms.

**An edit.** One stat (0.4 µs) and one read of 20 KiB (3.7 µs) take about 4 µs of kernel work.
If the driver is asleep, add one wake of about 5 µs, so about 9 µs.

**Today, before this pass.** A `ReadAll` of a file no request held went through the kept-file
path:

1. An `OPENAT` to a plain descriptor.
2. A `FILES_UPDATE` into a slot of the table.
3. An `fstat` system call on the driver's thread.
4. The `READ` into a zeroed buffer (`vec![0; len]`).

That is three trips through the ring and 3–4 SQEs. The file then stayed open by name, with
`O_RDWR | O_APPEND`, until the table evicted it, which cost a later `CLOSE` and a `close` call.

This was also a correctness gap. After another process renamed a new file over the name (an
editor's save, a `git checkout`), the next read got the old file's bytes. The threads driver
opens per read and got the new ones. Opening for writing also fails on a read-only file
(`EACCES`) and on a running executable (`ETXTBSY`), where a read should succeed.

Two more costs sat on the read path. Opens waiting in the ring held plain descriptors without
limit, so 50,000 queued reads could hit `EMFILE`. And every SQE ran inline on the one SQPOLL
thread: about 185 ms of kernel work on one core, against a 12 ms bound, about 15×.

`Stamp` already ran on the stamper threads in chunks of 64, at about 1.3 ms, which is at the
bound.

**What changed in this pass.** A read of a file no request keeps open for appending is now its
own request, `Kind::Load` in `uring/files.rs`. It is an `OPENAT` with `O_RDONLY` of the file at
its name when the read is asked, to a descriptor of its own. A `ReadAll` is then sized by `fstat`
on that descriptor. The bytes are read into the buffer's spare capacity, never zeroed first
(`spare_of` and `filled` in `uring/flight.rs`), and the descriptor is closed when the request
ends. That makes two trips and two SQEs a file. The read matches the threads driver after a
rename, and nothing is kept.

Reads holding a descriptor are capped by `loads_most`: half the driver's table, at most
`LOADS = 1024`. The rest wait in `load_waiting`, in the order they were asked. Once
`SPREAD_LEAST = 64` are open, each open and read carries `IOSQE_ASYNC`. Those then run on the
kernel's io-wq workers on every core, instead of inline on the SQPOLL thread. A read on a kept
file (an `Append`'s journal) still goes through its descriptor, into spare capacity as well.

The tests are in `uring.rs`, written in this pass but not yet run:

- `a_read_is_of_the_file_at_its_name_now`: a rename over the name, a missing file, an empty
  file, a short read at the end of the file, and nothing kept after.
- `reads_of_many_files_hold_few_descriptors`: never more than `loads_most` open, the flag set at
  `SPREAD_LEAST`, and each result equal to `std::fs::read`.
- `warm_reads_allocate_their_bytes_alone`: at most one allocation a read, and no calls on names
  on the driver's thread, counted with the seccomp filter (`path_calls`).

**The gap, and how the design reaches it.**

- **The driver's thread is the new limit for reads.** Each load costs it an `fstat` (0.15 µs), a
  `close` (0.15 µs), two SQEs and two CQEs, and a name pushed. That is about 0.8 µs a file, about
  40 ms for 50,000 files, against the 12 ms warm bound: about 3.3×. The SQPOLL thread still hands
  off 100,000 SQEs at about 0.3 µs each, about 30 ms.
- **Reaching 90% needs one effect for many reads, as `Stamp` is for stamps.** The parent lane
  owns `Effect`, and a request has gone to it. With 64 names an effect, the stamper-style threads
  do open, `fstat`, read and close blocking, on 16 cores. That costs the driver about 0.02 µs a
  file, so the reads come to about 12.5 ms, within 10% of the bound.
- **Without it,** the per-file fstat and close could become ring ops: a `CLOSE` linked after the
  read, and a size from the read's own short count instead of `fstat`. That would take the
  driver to about 0.4 µs a file (20 ms), still 1.7× the bound.
- **Whole writes are bound by the device.** The 6-SQE chain plus two `CLOSE`s puts about
  100 ms of copying on the SQPOLL core, under the device's 143 ms. Whether the per-write directory
  syncs are committed in groups is a measurement, not something to read from the code.
- **Two costs are off the read path now, but remain for kept files.** `wake` scans `on_open`
  with `Vec::remove`, which is O(n²) when many requests wait on one open. And `unkeep` calls
  `names.retain`, which is O(names).
- **An edit is within about 1.3× of its 9 µs.** The rest is the SQPOLL thread's idle wake, which
  is not worth closing.

**What proof's budgets should measure.**

- **50,000 `ReadAll` of 20 KiB with a warm page cache, on 16 cores:**
  - Wall time: ≤ 45 ms after this pass, and ≤ 13.2 ms with the batched effect.
  - CPU on the driver's thread a file: ≤ 1 µs, then ≤ 0.1 µs.
  - Peak descriptors: ≤ `loads_most` plus the kept files.
  - Calls on names on the driver's thread: 0.
- **Allocations a warm read:** ≤ 1 (its result). This is `warm_reads_allocate_their_bytes_alone`,
  kept as a budget.
- **5,000 whole writes of 200 KiB:** wall time ≤ 1.1 × `fio` with the same sync pattern on the
  same device.
- **An edit's stat and read, from submit to completion with the driver asleep:** ≤ 15 µs at the
  median.

**Allocation audit (2026-10-30).** `uring.rs` and `uring/` have 76 sites outside tests: 43
errors, 6 setup, and 27 per-open, per-spawn or per-read payloads or kept buffers. The ones
per completion:
- **A read's result** (`Vec::with_capacity(len)` in `start`, `files.rs`'s sized reads): the
  one allocation `warm_reads_allocate_their_bytes_alone` allows.
  - `ReadAll`'s bytes are kept by whoever asked (the VFS's text, the store's parse), so they are
    the payload.
  - A chunked `Read`, whose one caller appends each chunk to a buffer of its own, would allocate
    nothing with a `Recv`-style `into` buffer. That is queue 00942, because it changes the effect
    and blaze-usercode's caller in one step.
- **A file's name, cloned once per file opened** as the key it is kept under. Hits take the kept
  file and clone nothing.
- **A pipe's or a watch's buffer**, from the spares where one is kept.

`Effect::List` spawns a thread per effect (`off_ring`). Nothing outside blaze-core issues it, so
it is cold. A caller on a hot path would route it through the stampers first.

## The threads driver

`crates/blaze-core/src/threads.rs` is the reference driver. Its blocking calls (`threads::run`,
`fs.rs`) also serve kqueue's and IOCP's file effects.

**The work, per effect.** An effect is handed into a `Mailbox` (a mutex and a condition
variable), a pool thread makes the blocking calls, and the completion is handed back. That is two
mailbox operations, each an uncontended lock of about 50 ns. A futex wake (about 5 µs) happens
only when the other side is waiting, which is the minimum for a queue with one effect an entry.

**The bound.** The calls themselves:

- a `ReadAll` is open, size, read and close, about 3.7 µs for 20 KiB;
- an `Append` is open, write and close;
- a `Link` or `Remove` is one call that resolves a path (0.4 µs for a remove, about 0.8 µs for a
  link's two paths).

**Today, before this pass.**

- **`ReadAll`** is `std::fs::read`: open, `statx`, read, a probe read that finds the end, and
  close. That is five calls against four, and the probe is about 5% of the time. It stays,
  because reading to the end of the file is the reference's meaning of a whole file's read.
- **`Read`** zeroes its buffer first: a 20 KiB memset at 64 B a cycle, about 3% of the read. It
  stays.
- **`Append`, `Truncate`, `Write`, `Rename`, `Link` and `Clone`** each called `create_dir_all` on
  the parent before the call: one more `statx` that resolves a path, every time. That was about
  20% of a small append and about 50% of a link.
- **`Remove`** did `symlink_metadata`, then the unlink: two calls that resolve a path against
  one, about 50%.
- **`fs::stamps`** took at most 8 threads for each call: 2.5 ms for a 50,000-name `Stamp` on 16
  cores, 2× the bound. It now takes one thread per 2,048 names, up to the cores (`fs::sweepers`,
  as in the kqueue section below). A thread's spawn, about 20 µs against its 0.8 ms of
  `fstatat`, is a few percent.

**What changed in this pass.** `fs::making_parent` runs the call first. Only when it fails with
`NotFound` does it make the parent's directories and run the call again. `Append`, `Truncate`,
`write_atomically`, `rename`, `link` and `clone` use it, so a call whose directory is there makes
one call that resolves a path. `remove` unlinks first, and looks up only when the unlink fails
for some other reason: it removes a directory, or returns the unlink's error. The files and
directories left after each call are the same as before; only the extra lookups are gone. The
tests are `missing_directories_are_made_where_a_call_needs_them` and
`a_remove_takes_files_links_and_empty_directories` in `fs.rs`, written in this pass but not yet
run.

**The gap.** The calls are now at their count, except the reference's probe read and memset,
both under 10%. Handing off one effect at a time is at its minimum for an entry. A batched read
effect, as in the io_uring section, would cut the handoffs 64×.

**What proof's budgets should measure.**

- **Calls that resolve a path, per effect, on a warm tree:** `Append`, `Remove` and `Truncate` 1
  each, `Link` 1, and `ReadAll` 1 (its open). Counted with a seccomp filter on the pool thread,
  as `uring.rs`'s `path_calls` does.
- **Handoff cost:** ≤ 0.2 µs an effect when no side waits.

**Allocation audit (2026-10-30).** `threads.rs` has 41 sites outside tests: 19 errors, 4 setup,
and 18 payloads:
- a read's bytes;
- an exit's box;
- the `Arc` of a socket, a watch's control and a spawn's life, each handed to the pool thread
  that owns the work.

None runs per effect beyond its payload. Effects go to the pool as `Errand` values on a channel,
with no box.

## The kqueue driver

`crates/blaze-core/src/kqueue.rs` is the macOS driver. Sockets, pipes and processes are
registered with one kqueue, and their changes are batched into the next `kevent` call. File
effects go to the driver's file threads over a bounded channel (1,024 deep, with a backlog past
that), which hand completions back over a second channel. The timers are fix-maps' work, not
redone here: `wheel.rs` is a hierarchical timing wheel, and the one `EVFILT_TIMER` registered
per timer became the `Wheel`, with `kevent` waiting until its next deadline (equal deadlines
fire in insertion order).

**The work counted**, per effect and per batch of N: `kevent` calls, file calls on the
threads, handoffs between the polling thread and the threads with the wakeups they cost, and
allocations.

**The bound.**

- One `kevent` a batch, whatever N.
- One file call per file op, on its thread. A whole read (`ReadAll`) of a 20 KiB file is open
  0.4 µs, `fstat` 0.15 µs, one read of 20 KiB 0.15 + 2 µs (10 GB/s), and close 0.15 µs:
  2.85 µs.
- One handoff each way per batch. That is a channel op per effect, about 0.1 µs each way with
  the line moved between cores, and one wakeup of the polling thread for the batch, not one per
  effect.
- Nothing allocated warm but the asker's buffer: a read's bytes, a listing's entries, a
  stamp batch's `Vec<Stamp>`.

**The cold load against it**, on 16 cores with as many file threads:

- **50,000 stamps.** These are one `Effect::Stamp`, so one handoff each way. The work is one
  `fstatat` a name relative to the root (0.4 µs), 20 ms of core time, so the bound is
  20 ms ÷ 16 = 1.25 ms.
  - Before this pass `fs::stamps` took at most 8 threads: 2.5 ms, 2× the bound.
  - It now takes one thread per 2,048 names, up to the cores (`fs::sweepers`), as the IOCP
    sweep does (below): about 1.3 ms. The tests are
    `a_sweep_takes_a_thread_per_chunk_up_to_the_cores` and
    `a_sweep_on_many_threads_keeps_each_stamp_in_its_place` in `fs.rs`.
- **50,000 whole reads of 20 KiB.** The bound is 50,000 × (2.85 + 0.2) µs ÷ 16 = 9.5 ms. Today:
  - The file thread runs `std::fs::read` (in `threads.rs`). That is open, `fstat`, one read of
    the size, and a probe read of 32 bytes to see the end (0.15 µs, 5%), then close.
  - Before this pass, every completion also fired its own `EVFILT_USER` trigger: one `kevent`
    per file, 0.15 µs each, 5% more, plus a wakeup of the polling thread per file whenever it
    was asleep.
  - That made about 12% over the bound. The trigger is now one per drain (below), which leaves
    the probe read at 5%, within the bound.
  - Reading `size + 1` bytes into a buffer of that capacity would see the end without the
    probe, as the IOCP driver's `read_whole` does. `threads.rs` is speed-sol-core-uring's; it
    has the request.
- **5,000 whole writes of 200 KiB.** These are a temporary file, a write, `F_FULLFSYNC` and a
  rename, on the threads. The device bounds them: 1 GB at 7 GB/s is 143 ms, and each
  `F_FULLFSYNC` is a drive cache flush of milliseconds. The driver's share (a handoff, about
  10 path calls of 0.4 µs) is under 0.1%.
- **Accepts.** A listener's readiness is taken with a loop of `accept` until it would block.
  Each connection costs the `accept`, a `FIONBIO` and `SO_NOSIGPIPE` (three calls of 0.15 µs),
  plus one `accept` a batch that would block, and an `EV_ONESHOT` re-registration batched into
  the next `kevent`. That is within the bound: Darwin has no `accept4`, and whether its accepted
  socket inherits both flags is not yet tested.

**What changed in this pass.**

- **One trigger per drain.** `Notify` keeps a `handoff::Wake`. The first hand-back after the
  driver drains its channels fires the `EVFILT_USER` trigger; the rest of the batch fire
  nothing. `take` calls `drain` before it reads the channels, so a completion sent after the
  drain fires the next trigger and none is missed (the `Wake` protocol is model-checked in
  `handoff.rs`).
  - Test: `a_batch_of_file_effects_wakes_the_driver_once`. Three batches of 64 stats on 4
    threads; each batch is all handed back before the driver polls, and it fires exactly one
    trigger.
- **`set_nonblocking`** is one `ioctl(FIONBIO)`, not `fcntl` `F_GETFL` then `F_SETFL`: one call
  less for each pipe and socket made.
  - Test: `a_descriptor_made_nonblocking_has_the_flag_fcntl_reads`.
- The existing budgets hold: a warm file cycle allocates only the read's bytes, and a warm
  timer, times set or watch cycle allocates nothing but the changes.

**The edit.** One FSEvents batch, one stat and one read. They cost two round trips to a file
thread: if it sleeps, a wakeup of about 5 µs each, plus 0.4 and 3 µs of calls, so about 15 µs.
The watcher's own delivery (FSEvents' latency, a few ms even at latency 0 with `NoDefer`) is
over 99% of the edit, and blaze does not set it.

**What proof's budgets should measure.**

- `kevent` calls per batch of N completions from the threads: one per drain, not N. Today's
  test counts the triggers; proof should count all `kevent` calls with `dtruss` or a counting
  shim.
- Syscalls per `ReadAll` on a file thread: 4 once the probe read is gone (5 today).
- Allocations: the existing warm budgets, plus one per stamp batch.
- Wall time for the 50,000-file load on 16 cores, against 9.5 ms for the reads and 1.25 ms for
  the stamps.

**State of the art.** Watchman uses FSEvents the same way. `getattrlistbulk` returns every
entry of a directory with its attributes in one call, against a `readdir` plus an `fstatat`
an entry. `fs::entries` should use it: that is a request to core for `fs.rs`.

**Allocation audit (2026-10-30).** `kqueue.rs` and `fsevents.rs` have 55 sites outside tests: 22
errors, 7 setup, and 26 payloads or kept buffers.

One ran per spawn without need. `Kqueue::finish_spawn` formatted the program's name on every exit
just in case it failed; now it formats it only when the spawn failed.
`process::Finish::exited` names the program in its own reap and limit errors, so the wording of
every failure is kept, and the sandbox-trace error is no longer named twice.

FSEvents batches allocate one `FileName` per changed name (`Done::Changed`, `Journaled`). The
packed `journal::Names` would make that one buffer per batch; that is queue 00943, because the
VFS and cargo read those fields.

## The IOCP driver

`crates/blaze-core/src/iocp.rs` is the Windows driver. Its overlapped requests sit in a slab
with the `OVERLAPPED` inline in each entry. A packet finds its request by the key `KEY_IO`, and
`owner()` checks both the entry's generation and the head's address. Calls with no overlapped
form run on the driver's threads, which post one `KEY_WAKE` for all they hand back between two
drains. Readers and appenders are kept open by name, up to 256 of each. The timers are fix-maps'
work, not redone here: the `BinaryHeap` became the `Wheel`, and the port's wait is the timer.

**The work counted**: syscalls per effect and per batch, packets through the port, and
allocations.

**The bound.**

- One `GetQueuedCompletionStatusEx` for up to 64 completions.
- Overlapped `ReadFile` and `WriteFile` with no thread.
- `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS`: a call that ends at once posts no packet.
- `AcceptEx` with its socket made before the connection comes.
- Nothing allocated warm but the asker's buffer.

On Windows an open is the cost that matters. `CreateFileW` is about 10–20 µs with the file
system filters (Defender) a machine runs, against the table's 0.4 µs for a path call.
`GetFileInformationByHandle(Ex)` is about 0.3 µs, and `CloseHandle` about 1 µs.

**The cold load against it**, on 16 cores:

- **50,000 whole reads of 20 KiB.** The work is an open (15 µs), a size, one read (2 µs) and a
  close, about 18 µs, so the bound is 50,000 × 18 µs ÷ 16 = 56 ms.
  - Before this pass, `ReadAll` ran on the polling thread, one after another, and kept no
    thread busy. Each one was `CreateFileW`, an association with the port, `GetFileSizeEx`, and
    two overlapped reads, each posting a packet. That is about 20–25 µs each, serially, so
    1–1.25 s: 20× the bound.
  - It also read the wrong file. The handle kept by name still read the old file after another
    process's save had renamed a new one over it, so an editor's save was missed.
  - Now `ReadAll` runs on the threads through `files::read_whole`: open with
    `FILE_FLAG_SEQUENTIAL_SCAN`, size, one read into `size + 1` bytes that ends at the first
    short read, and close. The payload is its only allocation. With as many threads as cores
    that is the bound, plus a handoff of about 0.2 µs (1%).
- **50,000 stamps.** These are one `Effect::Stamp` swept by `iocp/stamp.rs`: per name,
  `CreateFileW`, two queries and `CloseHandle`, about 20 µs. That is 1 s of core time.
  - The bound is 62 ms on 16 cores and 16 ms on 64.
  - Before this pass the sweep took at most 8 threads: 125 ms, 2× the bound on 16 cores and 8×
    on 64. It now takes one thread per 2,048 names, up to the cores (`sweepers`).
  - A smaller bound needs `GetFileInformationByName` (Windows 11 24H2), one call of about 2 µs a
    name with no open: 50,000 × 2 µs ÷ 16 = 6 ms. Using it means raising blaze's minimum Windows
    version, since no-fallback rules out keeping the slower path for older Windows. That is a
    decision for the parent lane.
- **5,000 whole writes of 200 KiB.** These run on the threads: a temporary file, a write of 20 µs,
  `FlushFileBuffers`, and a rename. The flush, a device cache flush of milliseconds, is the cost.
  Each write's path allocations (about 10) and opens are under 0.1% of it.
- **Reads at offsets and appends on kept handles.** A read the page cache holds is a `ReadFile`
  of about 1–2 µs. Before this pass its packet (queued by the kernel, then dequeued by
  `GetQueuedCompletionStatusEx`) added about 0.3–0.5 µs, 20–25% over the bound.
  - Kept readers and appenders, and a `WriteAt`'s handle, are now associated with
    `FILE_SKIP_COMPLETION_PORT_ON_SUCCESS | FILE_SKIP_SET_EVENT_ON_HANDLE`.
    `Request::ask` returns whether the call ended at once (`sys::started`), and `put` ends such a
    request where it asked, from the result in its `OVERLAPPED`. Its next step (a short read
    going on to the end) is asked from there too.
  - Pipes, sockets, watches and the journal still always post, as each of their paths expects a
    packet.
- **Accepts.** `AcceptEx`'s socket is made when the accept is posted, before the connection
  comes, so that half of the bound is met. Only one accept is outstanding a listener, so a burst
  of N connections takes N trips through the port, about 15 µs each (a socket made, `AcceptEx`,
  a packet). The daemon's connection rate is far under that. Several outstanding accepts would
  complete in any order, so they wait on a workload that needs them.

**What remains in this lane's paths.** `Effect::Times` and `Effect::WriteAt` still open their file
on the polling thread: 15 µs of the poller each, serially, for a batch of N. Neither is on the
reference workloads; the next step is to open on the threads and hand the handle back.

**The edit.** The read is a `ReadAll` on a thread, and it now sees a file saved by rename, which it
missed before. Its cost (about 20 µs with the stat) is under 1% of the watcher's delivery.

**Tests.**

- `whole_reads_are_exact_and_see_a_file_renamed_over_theirs`: files of 0, 1, 4,096 and 65,539
  bytes are read exactly, a missing file reads as empty, and after a rename over a name whose
  reader is kept, the next whole read gives the new bytes.
- `a_warm_batch_of_whole_reads_allocates_only_their_bytes`: 64 reads of 20 KiB. The threads
  allocate at most one each, counted with `blaze_alloc_count::measure` around each file call on
  the threads. The polling thread allocates nothing.
- `cached_reads_that_end_at_once_post_no_packet`: 64 cached reads at offsets give their bytes and
  post fewer than 32 packets. A read past the end gives what is there.
- `a_sweep_takes_a_thread_per_chunk_up_to_the_cores`: the sweep's thread count, and that every
  thread's chunk is non-empty.
- The existing differential test against the threads driver (`script`) still covers whole reads
  of missing, linked and rewritten files.

**What proof's budgets should measure.**

- Packets per cached read: 0 when the cache holds it (the test's counter, `packets`).
- Syscalls per whole read on a thread: 4 (open, size, read, close).
- `GetQueuedCompletionStatusEx` calls per 64 completions: 1.
- Allocations: the payload per whole read, none on the polling thread.
- Wall time for the 50,000-file load on 16 and 64 cores, against 56 ms for the reads and 62 and
  16 ms for the stamps.

**State of the art.** IoRing (Windows 11) submits reads in batches like io_uring, but it has no
open, so the cold load's cost, the opens, stays. `GetFileInformationByName` is the stamp's state
of the art, as above.

**Allocation audit (2026-10-30).** `iocp.rs`, `iocp/` and `create_process.rs` have 144 sites
outside tests: 67 errors, 9 setup, and 68 payloads, kept buffers and per-spawn command lines.
None runs per completion beyond a read's bytes.
- The USN journal's per-record clones (`usn.rs`'s `Names`) are each one name the batch owns, or
  one name a directory is kept under. Queue 00943's packed `Names` removes them with the
  `FileName`s.
- The sweeps' path buffers are one per sweep thread, reused across names.

## The Linux watchers, fanotify and inotify

`crates/blaze-core/src/fanotify.rs` marks the whole file system (`FAN_MARK_FILESYSTEM`, with
`DFID_NAME` reports) and keeps two `Dirs` caches, `seen` and `known`, from a directory's handle to
its name. `watch.rs` is the inotify watcher, with a map from each watch descriptor to its span.
Both are read through the ring and hand out a `Batch`: the names sorted and deduplicated, as a
`Vec<FileName>`.

**The work.**

- **An edit:** one change batch. An editor's save is about 3–6 events (a temporary created and
  written, a rename).
- **A checkout of 50,000 files:** about 50,000 events.
- **Per event:** parse the header, the file handle and the name, look the directory up once, and
  make the name, which is the batch's payload.

**The bound.**

- **An edit:** one `READ` and its CQE (about 1 µs), plus about 50 ns an event, plus the wake
  (5 µs).
- **A checkout:**
  - copying 50,000 events of about 280 B: about 1.4 ms at 10 GB/s;
  - one lookup each: about 0.75 ms at 15 ns;
  - one name each, allocated: about 1 ms;
  - the sort and dedup: about 1 ms.

  That is about 4 ms in all.
- **inotify's setup:** one `inotify_add_watch` a directory, about 2 ms for 5,000 directories at
  0.4 µs each.

**Today.**

- **Reads:** one `READ` a batch.
- **Lookups:** one table lookup an event (fanotify's `Dirs` caches, inotify's map).
- **Allocations:** only the payload.
- **fanotify** walks each event's bytes twice, once to frame the events and once to read them:
  about 20 ns an event, under 5%.
- **inotify** copies each name twice, into the path buffer and then into its `FileName`: about
  the same.

Read from the code, both watchers are within about 15% of the bound. Nothing in them is a gap of
more than 10% that the code shows, so they did not change in this pass.

**What proof's budgets should measure.**

- **A 50,000-file checkout under fanotify:**
  - CPU from the `READ`'s CQE to the `Batch`: ≤ 4.5 ms.
  - Allocations: ≤ 1 per name, plus the batch's `Vec`.
- **An editor's save:** ≤ 10 µs from the kernel's event to the batch's completion, with the
  driver asleep.
- **An inotify setup over 5,000 directories:** ≤ 2.5 ms.

## The macOS and Windows watchers, FSEvents and ReadDirectoryChangesW

`crates/blaze-core/src/fsevents.rs` watches a tree with an FSEvents stream, and
`crates/blaze-core/src/iocp/watch.rs` watches a directory with `ReadDirectoryChangesW` on the port.

**The work counted**: syscalls and allocations per change batch.

**The bound.** One callback or completion a batch. Nothing per event but a table lookup and the
name. One rearm a batch.

**FSEvents, today.**

- One callback a batch, with latency 0 and `NoDefer`.
- Each event's path is a span appended to one reused `String`. The spans are sorted and
  deduplicated before any `FileName` is made, so an event costs a copy of its path, and a name
  changed many times in a batch is made once.
- The batch goes to the driver over a channel, and its wakeup is now one trigger per drain (the
  kqueue section).
- FSEvents needs no rearm.

That is the bound.

**ReadDirectoryChangesW, today.**

- One completion a batch, and one rearm into the same buffer.
- Each record's name is turned from UTF-16 into a `FileName`: the name.
- Above the bound, each name that came in (added or renamed to) costs a `GetFileAttributesW`
  (a path call of about 5–15 µs) to learn whether it is a directory, and a directory that came in
  is walked.
- An editor's save is about 3 records, so about 30 µs: over the bound for the batch, but under
  1% of the edit, which the watcher's delivery bounds at milliseconds.
- `ReadDirectoryChangesExW` (Windows 10 1709, within blaze's minimum, which AF_UNIX already sets
  at 1803) gives each record's attributes, so the call would go. Those attributes are as of the
  event, though, while today's are as of now. A directory that came in and was removed would be
  walked rather than lost. The change waits on a differential test of the two over replayed
  batches.

**The cold load.** Neither watcher is on it.

**What proof's budgets should measure.**

- Callbacks or completions per batch: 1.
- Rearms per batch: 0 for FSEvents, 1 for ReadDirectoryChangesW.
- Allocations per batch: the changes' list and one per distinct name (the existing budgets
  `a_warm_watch_allocates_only_its_changes` and its kqueue twin).
- Path calls per came-in name: 1 today, 0 after `ReadDirectoryChangesExW`.

**State of the art.** Watchman watches the same way: FSEvents on macOS and
ReadDirectoryChangesW on Windows. The USN journal (`iocp/usn.rs`, lane core's journal) is the way
past both, for a whole volume at once.
