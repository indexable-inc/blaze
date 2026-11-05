---
status: plan
summary: mem-core-plane - blaze-graph's plane, pool and pump onto blaze-mem - node, task and request slots as generational Slabs over RegionVec columns, job cells from fixed-size Pools with a cache per thread and an intrusive inject queue, waiters linked from their own stacks, the bounds each now has, the speed of light of each, and the allocation-budget tests that hold them.
date: 2026-10-29
---

# mem-core-plane: the plane, the pool and the pump onto blaze-mem

Lane `mem-core-plane`, a child of [mem-core](../core.md), moved the per-task, per-request and
per-job allocations of `crates/blaze-graph` onto `crates/blaze-mem`: `plane.rs`, `pool.rs`,
`pump.rs` and their tests. `machine.rs` needed nothing. The plane's design is
[core/plane.md](../../core/plane.md), "The plane" and "The executor"; this says only what
changed and why. Written without a build (the lane's brief allows none): the first `blaze test
-p blaze-graph` is the check, and the tests below are what it runs.

## The plane

**Slots.** A node, a task and a request each take a slot of a `Slab` of a zero-sized marker
(`NodeSlot`, `TaskSlot`, `RequestSlot`, all tagged `Tag::Plane`). The slab is both the
generation column and the free list, so whatever holds a slot holds a `Handle`, index and
generation: the id index (`NodeId` to `NodeHandle`), the request index, the keys in flight,
`NodeRow::task`, the held task, the idle request's running tasks and the tasks passed over. A
stale handle is seen as such: a `Done` for a node whose task has ended and whose slot another
task took is refused with `Error::NotRunning`, and `running_task` checks the handle's
generation as well as the node's state.

**Rows.** The rows stay structure of arrays, indexed by the handle's index: `rows`, `cold`,
`states` and `keys` for nodes, `tasks` and `task_keys` for tasks, and `requests`. Each is a
`RegionVec` reserved up front to its bound and committed as it grows, so no column moves or
reallocates. A slot's rows are pushed the first time the slot is made and written over after
that.

**Order.** The slab's free list puts the last freed slot first, as the `Vec` free lists it
replaces did, so every slot, and hence every output, is the one the old plane chose. The
reference plane comparison and the 600-seed simulation hold that as before.

**Row sizes.** `NodeRow` stays 64 bytes: it gains `task: TaskHandle` (8 bytes, for the old
`u32`) and gives `path_ns` (read only when a node settles) to `NodeCold`, which grows from 56 to
64 bytes. `TaskRow` stays 32 bytes. Both sizes are `const` assertions.

**Scratch.** Every buffer a step uses is kept, as before, so a warm step allocates nothing.
`after`'s radix sort of the touched requests now sorts with a kept scratch
(`sort_copy_by_key_in`), where it allocated one above the sort's small size. A request's slot
keeps its heaps, edges, names and running set for the next request, so a long-lived plane
holds what its open requests hold.

**Bounds.**

| Slots | At most | Past it |
| --- | --- | --- |
| Nodes | `MAX_NODES` = 2^24 live at once | the step stops with the slab's `MemError` naming `plane` |
| Tasks | `MAX_TASKS` = 2^24 | as for nodes |
| Requests | `MAX_REQUESTS` = 2^16 | as for nodes |

The old plane asserted the same 2^31 bound on every index (`NEW` marks a place of the batch
being checked). The new bounds are below it, with a `const` assertion, and reserve address
space only: 2^24 64-byte rows is 1 GiB reserved, nothing committed until used.

**What still allocates.** Only payload: the path of a request's one `Event::Critical`, which
is the event's own `Vec`. Error and refusal messages (a cycle, a node too big for the budget,
a cancel note) and the outputs of `Plane::step`, `Plane::cancel` and `Machine::step` are
allocated where they are made. Each such site carries an `// alloc:` comment.

**Speed of light.** A step's cost is the work of its rules: one hash lookup per node id, a
handle compare per slot read, and row reads in 64-byte lines. Taking a slot is a free-list
pop, freeing one a generation bump and a push, the same as the `Vec` free lists plus one
compare per lookup. The generation check is the one cost added, and it is what catches the
stale handle.

## The pool

**Cells.** A job's closure goes in a cell of 128, 512 or 2048 bytes, 64-byte aligned, from one
of three process-wide `blaze_mem::Pool`s (`Tag::Plane`). Cells are process-wide, not one
pool's, because a pool's worker that pumps may run another pool's job. Each thread, worker or
not, has a `Cache` of each size in a thread local: a submit takes a cell from the submitter's
cache, and a run gives it back to the runner's cache before the closure runs.

The job is a `Placed<F>`: its `Header`, the cell's own `Pooled` (as `Room`), then the closure.
The cell size is `Placed::<F>::CLASS`, a `const` evaluated when `F` is compiled. A closure no
cell holds (past 2 KiB with its 48-byte header, or aligned past 64 bytes) fails the build; a
job is never boxed. The callers seen are 40 to 60 bytes (radix's `run_jobs`, base-db's
`run_on_pool`, exec's machine scope), which take the 128-byte cells, and about 190 bytes
(blaze-cargo's `serve/plane.rs` posting), which take the 512-byte cells. exec's scheduler
places its own runs with `Job::from_header` and takes no cell.

**Inject queue.** crossbeam's `Injector` made a block per 63 pushes. A group's queue is now
`Inject`: a list through each job's new `Header::next` behind a `Lock`, with an atomic length
read without the lock (Tokio's inject queue). The jobs are its nodes, so a push allocates
nothing. A steal takes up to `BATCH` jobs under the lock, runs the first and gives the rest to
its deque after the lock is let go. `Header` grows from 8 to 16 bytes (`Header::new` stays
`const`); exec's `Run`, whose first field it is, grows with it.

**Bounds.** 2^22 small, 2^18 medium and 2^16 large cells live at once: 512 MiB, 128 MiB and
128 MiB of address space, reserved by the first `Pool::new`, which says so if it cannot be. Past
a bound, a submit stops with the pool's `MemError`.

**Speed of light.** A warm submit is a cache pop, a write of the closure, and either a lock,
a link and an atomic add (inject) or a swap into the LIFO slot. A run is two reads, a cache
push, then the closure. One compare-and-swap on the cells' shared stacks per 64 cells moved
between threads. What is left is crossbeam's `Worker` deque: it grows its buffer on a burst
past 64 jobs and shrinks it again, both allocations, and only on bursts.

## The pump

**Waiters.** A wait for a run is a `Waiter` on the waiter's own stack, linked into its run's
`InFlight` entry under the key's shard lock. When the run ends, the runner takes the entry out
under the lock and walks the list twice: first to clear every waiter's edge, then for each
waiter it reads `next` and the thread, writes the value's clone, stores the state (release),
and wakes the thread. After the store the runner touches the waiter no more. The waiter waits
until it reads the state (acquire), then takes its value. A `Linked` guard holds the waiter in
place should the wait unwind: it unlinks the waiter if the entry still holds it, or waits for
the runner, which has the list. `Arc<Flight>`, its `OnceLock` and the waiters' `Vec` are gone.
An `InFlight` is 16 bytes (it was 56), and a value is cloned once per waiter (it was twice:
once into the flight and once out of it).

**Thread slots.** The free list of thread slots threads through the slots' own `Stack`s (a new
`next_free`, which fits in the 384-byte stack) under `FREE`, last freed first as the `Vec` it
replaces was. `next_free` is `std`'s atomic even under `blaze_check`, since `FREE`'s list
outlives an execution.

**Speed of light.** A run nobody waits for takes its shard twice (insert, remove) and nothing
else. A wait adds the cycle check under `WAITS` (kept scratch), one link, and on the end one
store and one unpark per waiter.

## Tests

| Test | Where | Holds |
| --- | --- | --- |
| `a_warm_request_allocates_only_its_critical_path` | `plane_tests/memory.rs` | a warm request (open, add a diamond with a keyed node, close, release, every Run, Done and Finished) makes exactly one allocation, its `Event::Critical` path, and reuses the last request's slots |
| `a_done_for_a_task_whose_slot_was_reused_is_refused` | same | a second `Done` for a node whose task's slot another task took is `Err(NotRunning)`; the old handle is stale while its index is the new task's |
| `the_slots_take_what_the_free_lists_took` | same | over 200 seeds, `Slots` takes the index the old free list took, and a handle is live exactly when the model says |
| `a_warm_job_allocates_nothing` | `pool.rs` | on a warm one-worker pool, a scope of 8 jobs submitted, run and waited for allocates nothing |
| `a_warm_submit_from_outside_allocates_nothing` | same | 64 submits from a thread of no pool allocate nothing |
| `a_job_in_a_cell_runs_as_the_same_closure_boxed` | same | over 20 seeds, jobs of all three cell sizes give what the same closures boxed give, run once, and drop their captures once |
| `a_job_takes_the_smallest_cell_that_holds_it` | same | the cell size of a closure, up to the largest a cell holds |
| `a_miss_then_a_hit_allocate_only_the_values_clone` | `pump.rs` | a run with a waiter: no allocation for a `u64`, exactly the one clone for a `Vec` |
| `every_waiter_of_a_run_gets_its_value` | same | eight waiters linked into one run each get its value |

The existing tests stay: the 600-seed simulation, the comparison against the reference plane,
the pool's seeded stress and the pump's random waits. The `blaze_check` models (`model_` in
`pool.rs` and `pump.rs`) now run the inject list, the job cells and the linked waiters.
