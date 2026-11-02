# Requests from lane core-pool

## 1. `Cargo.lock`: blaze-graph's new dependencies

- What: `crates/blaze-graph/Cargo.toml` now depends on `crossbeam-deque` (workspace) and, on
  unix, `libc` (workspace). Both are already in `Cargo.lock` (crossbeam-deque 0.8.6, libc
  0.2.186); only blaze-graph's `dependencies` list in the lock gains the two names.
- Where: `Cargo.lock`, the `blaze-graph` package entry.
- Why: the worker pool's deques (lock-free push, pop and steal) and the topology's
  `sched_getaffinity` / `sched_setaffinity` (Linux) and `sysctlbyname` /
  `pthread_set_qos_class_self_np` (macOS).

> applied: nothing to edit by hand: `crossbeam-deque` and `libc` are root workspace dependencies and both packages are in `Cargo.lock`; the first compile adds the two names to `blaze-graph`'s entry (no new package).

## 2. One `Cancelled` for the query database and the pump (lane db, coordinator)

- What: `blaze_query::Cancelled` (crates/blaze-query/src/lib.rs) becomes a re-export of
  `blaze_graph::Cancelled` (`pub use blaze_graph::Cancelled;`), replacing its own struct.
- Where: `crates/blaze-query/src/lib.rs`.
- Why: `Cx::get`'s "reused, waited on if running, or run inline" is
  `blaze_graph::pump::Flights::get_or_run`, which returns `blaze_graph::Cancelled`; the
  scheduler's `Work` returns it too. Two types for one thing would need a conversion at every
  `?`.

> applied: by db (`pub use blaze_graph::Cancelled;` in `crates/blaze-query/src/lib.rs`).

## 3. docs/v2/plan/lanes/core/plane.md "The executor" (core)

- What: say what the executor now is: groups are the CPUs of a level 3 cache or, where the
  machine says so (a cluster of more than one core), a cluster, the smaller winning; on macOS a
  group is an L2 cluster of one performance level (not "the performance and efficiency
  cores"), level 0 fast, and a job near no group goes to the least loaded group, fast first.
  Name `blaze_graph::pump` as #1301's rule (pump while waiting, cancel the wait that closes a
  cycle) and `exec::Scheduler` as the owner thread. `drive` runs a build's work and IO nodes on
  two pools lent to it alone and kept for the next build of their size, so a build starts no
  threads.
- Where: `docs/v2/plan/lanes/core/plane.md` (core's path).
- Why: docs-say-what-they-are; the section was written before the pool.

> applied: by core (`docs/v2/plan/lanes/core/plane.md`, "The executor").

## 4. pool.rs's part of core-hardware.md (speed-runtime-pool, while it rewrites pool.rs)

core-hardware.md gives core-pool three pool.rs items. speed-runtime-pool is rewriting pool.rs,
so core-pool's own edits to it were overwritten; to avoid a third edit war, the rewrite takes
them in. `topology.rs` (NUMA) and `exec.rs` (the callers) are done by core-pool already.

- What:
  1. **`pub fn node() -> Option<u32>`**: the calling worker's NUMA node, `None` off a pool.
     `Cpu` now has `node: u32` (topology.rs); keep it per slot, as `group` is.
     `arena.rs` (core-plane) binds huge-page chunks to `crate::pool::node()`.
  2. **`Pool::submit_critical(near, run, done)`**: same signature as `submit`. exec.rs's
     `post` calls it for a task the plane's `Run { critical: true, .. }` names, and plain
     `submit` otherwise. Placement:
     - Only when the pool has both fast and slow groups (`mixed`) does `critical` matter.
     - A critical job goes to `near` if that group is fast, else the least loaded fast group.
     - A non-critical job goes to `near`, or to the least loaded of all groups (fast first on
       a tie, then the lower id).
     - On an all-fast machine a critical job is placed exactly as a non-critical one.
     - Stealing is unchanged: an idle slow worker may still take a critical job, so no worker
       idles while there is work.
  3. **Tests**:
     - Placement checked on `Shared`'s choice directly, on
       `Topology::from_perflevels(&[(4, 2), (4, 4)])`. Run tests can't check it, because
       stealing makes where a job runs depend on timing.
     - A worker's `node()` matches its group's, on a two-node `Topology`.
     - The `Cpu` literals in pool.rs's tests gain `node: 0`.
     - A `#[cfg(test)] pub(crate) fn critical_submitted(&self) -> usize` counter, which
       exec.rs's test uses to see the plane's flag reach the pool.
- Where: `crates/blaze-graph/src/pool.rs`.
- Why: core-hardware.md ("P cores for the critical path", "NUMA"); hardware.md's threads and
  memory rows.

> forwarded to speed: speed-runtime-pool is rewriting `pool.rs`.

Status: done by speed-runtime-pool (2026-10-29). `pool::node()`, `Pool::submit_critical` (and
`pub(crate) push_placed`, which exec.rs's run table uses for its intrusive jobs), placement as
specified (`Shared::place`, `least`), `critical_submitted`, the `node: 0` test literals, and
tests: `least` against a model, placement on `from_perflevels(&[(4, 2), (4, 4)])`, all-fast
unchanged, `node()` on a two-node topology.
