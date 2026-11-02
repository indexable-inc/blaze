---
status: plan
summary: lane sched's speed of light for the scheduler core (crates/blaze-graph/src/sched/): the cache lines and instructions each step must touch, what the code touches, and the bound for steals, wakes, moves and cycle checks.
date: 2026-10-31
---
# sched: speed of light

Each step of the core (`crates/blaze-graph/src/sched/mod.rs`) does a fixed amount of work: it
reads and writes a few words. The bound for a step is the cache lines it must touch, plus, for
anything that crosses cores, one coherence transfer per line another core wrote.

## The steps

| Step | Lines it must touch | What the code touches |
|---|---|---|
| local spawn | the job's record, the queue's tail, the core's stealable bit | the job's 32-byte hot `Job` record (one line), the `VecDeque` tail, one bitmask word; `balance` reads `parked_count` (one line, shared) |
| counted spawn | the above, plus the counter's count and list head | the job's 20-byte cold record and the counter's columns |
| pick from own slot or queues | the core's slot or queue head, the job's record | the same, plus one `Decision` pushed into a reused `Vec` |
| `done` with no waiters | the job's record, then a pick | the same; waiter and claim lists are `NONE` heads in the same record |
| `done` with k waiters | the record of each waiter (k lines) | k records, linked through their own `next` field (no side allocation) |
| suspend | the job's record, the target's record (to link) | the same, plus the cycle check below |
| parallel map of n items | n spawns, one suspend, one wake | the same: the map's job suspends once on the counter, never once per item |
| steal | a mask word per tier, the victim's queue | up to 5 levels' tiers × ⌈cores/64⌉ words ANDed, then half the victim's queue moved |
| wake to a parked core | the parked mask, the target's slot | `nearest_parked`: the same mask walk (with a capacity filter word), then one store |
| move request | the parked mask, the big mask, the critical-small and yield-sent masks | `maybe_yield`: a word scan, run only when a fast core parks or a critical job starts on a slow one |

At L1 latency (about 1 ns a line) and a few instructions a line, a spawn, a run and an end of a
job on one core cost 10 to 15 ns together in the core model, which is 70 to 100 million jobs a
second per core. Rayon's `join` on a warm deque costs about 10 to 20 ns, and Tokio's spawn and
run about 50 to 100 ns; the core's local path is the same order of work as Rayon's.

Across cores the bound is the coherence transfer: a steal moves the victim's queue head line,
which costs 50 to 100 ns within an L3 group and 150 to 300 ns across sockets. The
nearest-first order, by the kernel's distance table, keeps steals on the cheap side. Stealing half amortises the transfer over the jobs taken.
A wake of a parked core costs one futex wake in the driver (1 to 2 µs), paid only when a core
is parked; the driver spins briefly before parking, so a core that would be woken at once
never sleeps. Work conservation means that is also the only time a core sleeps while work
exists.

A suspend and resume on a fiber is two register-file switches (about 10 to 20 ns each) plus the
core's suspend and pick.

## Where the code is off the bound, and why

- **`least_loaded` scans every core**, but only for a spawn with no spawner that names a group,
  a node or `Any`: the driver's roots, a handful per build.
- **The cycle check is a DFS over suspended jobs**: on the common path the target is running
  or queued, and the check stops after one load. A deep chain of waits costs one record load
  per job on it, and a counter target one per live item. A refused wait allocates its `Cycle`,
  the only allocation outside setup, on an error path.
- **The core is one state machine.** As a runtime it would serialise every step on one lock.
  Phase 2 splits it by owner (a Chase-Lev deque and a `runnext` slot per core, atomic masks),
  so a run that nobody waits for touches only its own core's lines, and only waits take the
  wait table's lock. That is how the runtime reaches the per-step bound above on every core at
  once.

## Allocation

Zero on the warm path, checked by `a_warm_scheduler_allocates_nothing`. Every column is sized
at `Sched::new(cores, jobs)`. Queues are `VecDeque`s that keep their capacity; waiter, claim
and counter lists are intrusive (`u32` links in the records); key flights and counters are
reused free lists beside an `FxHashMap` sized for `jobs`; timers are `blaze_core::wheel::Wheel`'s
slab; and the DFS stack, the scratch buffer and the decision buffer are reused.
