---
status: plan
summary: speed-runtime-pool's pass over blaze-graph's worker pool, pump and scheduler run path (pool.rs, pump.rs, exec.rs; topology.rs read only): the state of the art for each, how the code compared, what changed and how it compares now.
date: 2026-10-29
---
# speed-runtime-pool: the pool, the pump and the scheduler's runs

What `speed-runtime-pool` changed in `crates/blaze-graph/src/pool.rs`, `pump.rs` and `exec.rs`
for the speed bar of `docs/v2/plan/goals/principles.md`. `topology.rs` runs once, at startup,
and is unchanged here (core-pool added NUMA nodes to it). Nothing was built or run; the counts
below are read from the code and from the crates it uses.

## The worker pool (`pool.rs`)

**Benchmarks.** Chase-Lev deques as in Tokio, Rayon and Go; Tokio's LIFO slot
(`MAX_LIFO_POLLS_PER_TICK = 3`); Go's and Tokio's cap on spinning workers (at most half
searching, and the last searcher to find work wakes one more); stealing nearest first; one
futex per parked worker.

**How it compares now**, per operation, against `crossbeam-deque` used plainly and Tokio's
multi-threaded run queue:

| Operation | blaze pool | Tokio | plain crossbeam |
|---|---|---|---|
| push to own deque | 0 RMW, a store of `back` (crossbeam) | 0 RMW, a store of `tail` | same as blaze |
| push to LIFO slot | 1 `swap` on the worker's own 128-byte slot | a plain store (its slot can't be stolen) | none |
| pop own | LIFO slot: a load, then 1 `swap` if it holds a job. Deque (FIFO): 1 CAS on `front` | 1 CAS on `head` | 1 CAS |
| steal | 1 CAS on the victim's `front`; up to half its jobs copied, so 1 line of the victim written | 2 CAS (claim, then release the steal head) | 1 CAS |
| wake | none while any worker searches. Otherwise 1 CAS on the sleeper's slot, 2 RMW on the idle word, 1 futex wake | same rule, 1 futex wake | none built in |
| park | 1 futex wait on the worker's own thread (`thread::park`) | 1 futex (its condvar or driver) | none built in |

- **Per-worker state** (`Slot`, `Group`, `Idle`, a scope's `Pending`): each is 128-byte
  aligned, and a test asserts it. A job is 8 bytes (`Job(NonNull<Header>)`), so one line of a
  deque's buffer holds 8 jobs.
- **Allocation.** `submit` boxes its closure once, as Tokio allocates a task cell per spawn.
  The scheduler's runs don't go through `submit`: they are intrusive (see below) and allocate
  nothing.
- **Where it still trails.**
  - The LIFO slot costs one `swap` where Tokio's costs a store. Ours can be stolen, so an idle
    worker never waits behind a busy worker's slot. That RMW is on the worker's own line.
  - The group injector is crossbeam's `Injector`, which allocates a block every 63 pushes.
    Tokio's injector is a mutex-guarded list; Go's is a locked global queue. Ours takes no lock
    but allocates; a ring sized to the pool would remove that.

**Changed in this pass:**
- **`submit_critical`.** On a pool of fast and slow groups, a critical job goes to its `near`
  group when that is fast, else to the least loaded fast group. Where every group is fast it is
  placed as `submit` places it, and stealing is unchanged. This is core-hardware's P-core rule,
  which core-pool handed to this lane.
- **`pub(crate) push_placed`** puts an intrusive job on the pool with the same placement.
- **`pool::node()`** names the calling worker's NUMA node, kept in its `Local`: no atomic is
  read.
- **Placement is one pure function**, `least`, over each group's (load, workers, fast). Loads
  are compared without division, with no per-job count: load is read from what the injector
  and the sleepers already keep.

## The pump (`pump.rs`)

**Benchmarks.** rustc's query latches and salsa's blocking: the dependency graph is touched only
when a query actually blocks. Single flight by sharded table as in Go's `singleflight`, but
sharded.

**Before** (core-pool's first version): one process-wide mutex held the whole wait graph. It
was taken when a task started and ended and when a wait began and ended, so every
`get_or_run` took it two to four times. On 100 cores every query contended on one line.

**Now:**
- **No lock of the process on the common path.** A key not in flight takes its shard twice.
  The shards are a power of two, at least 4 per core and at least 64, each 128-byte aligned.
  A run allocates nothing once the shard's table has grown: its `Flight` is made only by its
  first waiter.
- **Each thread publishes its stack of tasks** in a 384-byte, 128-aligned `Stack` that only it
  writes. Starting a task is three stores to it and ending one a fourth.
- **The cycle check reads those stacks without a lock**, and the global `WAITS` lock is taken
  only when a wait begins, to check its edge and add it as one step.
- **Keys index directly.** `KeyBits` takes a key's first 8 bytes as its hash, and the shard is
  picked by bytes 8..16, so no key is hashed again.
- The test `a_run_nobody_waits_for_takes_no_lock_of_the_process` counts `WAITS` acquisitions
  over 1000 nested runs and expects none.
- **Kept:** `PUMP_DEPTH = 16`, the rule that the asker whose new wait would close a cycle gives
  way, and the panic when a task asks for its own key.
- **Where it still trails:** a shard is a mutex, so a run is two uncontended lock RMWs. A
  lock-free table could do a claim with one CAS, but its entries would need epoch reclamation,
  which costs more than it saves at this rate.

## The scheduler's runs (`exec.rs`)

**Benchmark.** Tokio's task: a header first, so the queue holds the task itself. One message per
completion, and nothing allocated per task in steady state.

**Before:** each run made an `Arc<Job>`. It cloned the work's `Arc`, the owner's `Sender`, the
IO queue's `Sender` (for IO tasks) and the `Job`'s `Arc` for `done`, then boxed both closures
in `submit`. That is two allocations, freed on the worker (a cross-thread free). It is also
four atomic increments and four decrements, on lines that every worker's run shares.

**Now:**
- **A run is a slot of the owner's `Table`.** Chunks of 64 never move, and a slot is reused
  once its `Done` is in.
- **The slot is the pool's job.** Its `Header` comes first (`repr(C)`) and runs `run_pooled`,
  so posting a run allocates nothing. The table grows to the most runs ever under way at once.
  Two tests check this: `runs_reuse_their_slots` (1000 runs fit one chunk) and
  `dropping_the_scheduler_waits_for_its_runs`.
- **No `Sender` is cloned.** The senders sit in a context leaked once per scheduler (`Ctx`),
  because a run's last act is a send whose end nothing can wait for.
- **The work's `Arc` is cloned and dropped by the owner thread only**, so its count line stays
  on that core.
- **A run's end is one `Msg::Done`**, carrying its slot index.
- **Behaviour change:** dropping a `Scheduler` now waits for its runs under way, as it already
  did for IO tasks. Its doc says so, and that it must not be dropped by one of its own tasks.
  The IO threads stop on a `None` from the owner, sent even if the owner unwinds.
- **Where it still trails:** the owner's channel is crossbeam's unbounded list, which allocates
  a block every 31 messages. A bounded ring would remove that, but a worker could then block on
  a full queue, which concurrency.md forbids on a worker.

## `drive`

`drive` (the one-request `Machine` path) still boxes each node's scope job once
(`Job::erase`). Its pools are lent and reused, so it starts no thread. Making it intrusive the
same way would need a slot table that outlives the scope's borrow.
