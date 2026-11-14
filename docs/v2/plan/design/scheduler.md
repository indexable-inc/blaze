---
status: plan
summary: The scheduler blaze is moving to: one graph of pure, identity-keyed nodes pulled by what is waited for, cycles as single nodes, no shared mutable state, priority from who waits, memory and cores both admitted, code tiers as nodes, built to scale to hundreds of cores. Replaces the 2026-11-02 plan, with what of it is built and the order of the rest.
date: 2026-11-11
---
# The scheduler: one graph of keyed work, pulled by what is waited for

This replaces the plan of 2026-11-02 (this file at `7e14a1559d`, which holds that day's
measurements and its survey of Buck2, Skyframe, salsa, Shake and the scheduling literature). That
plan's diagnosis stands: the pool is sound (work-stealing deques, fibers that free their thread on
a wait, a wait graph that refuses cycles), and the scheduling model is what is wrong. What changed
is the goal. blaze now has to (a) run on hundreds of cores, (b) start a program or a test on
unoptimized code and optimize behind it, as a JIT does, and (c) fit its memory to the machine.
The old plan had no place for (b), treated (c) as eviction only, and proved (a) to 64 cores.

## What is measured

A release build of blaze by blaze, 2026-11-11, tree `cf9404a18383`, 64 cores, capped at 28 jobs,
stopped at its 99 GiB ceiling after 35 minutes (profile on `rework-d-1110`, `/root/boot/h1`):

| what | measured |
|---|---|
| cores busy (self CPU per 3-minute window) | 17 to 43 of 64 in the main stretch; about 5 at the start, 3 at the end |
| CPU by span | `mir SpendFacts unwind` 13,388 s (about 30%), `codegen describe` 7,934 s, `llvm compile` 6,046 s (about 13%), inference 4,689 s, monomorphization 3,424 s |
| the effect-summary herd | 36 runs of `SpendFacts unwind` over 60 s each (4,573 CPU-s); 30 listed, all starting in one minute on 30 threads, 102 to 141 s each |
| one lock | `codegen.devirt.program`: 6.46 million waits, 5,111 thread-s; `codegen.target.named`: 2.65 million, 1,444 thread-s |
| waiting on another thread's query | `crate_items` of one crate 3,568 thread-s; several others 1,400 to 2,700 each |
| waiting for a work thread | 3,576 s over 493 tasks |
| memory | footprint rose from 19 to 99 GiB and never fell |

Not measured: parked-with-work time for this run, and the full 80-minute run on 256 GiB (its
profile kept only the last five minutes of task spans; that loss is its own bug).

Of the 2026-11-02 plan's steps, by a search of the tree on 2026-11-11: the idle-with-work measure
and the transcript simulator exist (`blaze-profile`), write-once slots and the per-crate `impls`
maintainer exist (`blaze-query`'s `slot`, `hir-ty`'s `method_resolution/delta`). Still present:
`cost: 1` in `blaze-cargo`'s `serve/job.rs`, the level loop in `blaze-codegen`'s `driver.rs`,
`layer()` in name resolution, no strongly connected components in `hir-ty-pure`'s `walk.rs`, no
bands in `blaze-graph`'s `sched`.

## The model

1. **One graph, one kind of node.** A node is a pure function and its arguments, keyed by the
   function's code identity and the arguments' content (`cache-by-code-id`). Its result is a
   record written once into the store (`outputs-are-records`). A crate, a unit and a phase are
   questions asked of the graph, never things the scheduler queues. The scheduler forms batches
   itself, from predicted cost, so that a batch is worth its dispatch: no author decides
   granularity by where a `par::map` happens to sit.
2. **Pull.** A node exists because something waits for it. The roots are goals: the diagnostics
   of an edited file, a runnable program at tier 0, one test's result, optimized code for a
   function a running program calls often. Demand flows down the graph; nothing is computed
   "because the phase got there". Speculation (warming what the next edit probably needs) is
   allowed, named, and in the lowest working band.
3. **A cycle is a node.** When a walk finds that keys depend on each other (the effect summaries
   of mutually recursive instances, trait-solver cycles), the strongly connected component
   becomes one node that owns its members: one task iterates it to its fixpoint and publishes
   every member's record at once; any other asker waits on the component's key. No second thread
   ever runs a member. This removes the summary herd above and the class of race behind the
   `summary_in` panic of 2026-11-11.
4. **No shared mutable state in a node.** A node reads records and write-once slots and writes
   its own record. What is now a lock around a growing structure (`codegen.devirt.program`, the
   named-target table) becomes records keyed by what they are about (one per trait method, one
   per symbol), read without a lock. A node never holds a lock, a thread-local borrow or an `Rc`
   across a wait (`no-wait-across-thread-locals`).
5. **Waits are per key, on atomics.** Single flight is per key and holds across every build in
   the daemon; the store extends it across daemons and machines, since the key is the same
   everywhere. A wait publishes what it waits on, walks owners on atomics with a bound, and is
   refused if it would close a cycle that rule 3 did not already make a node. No process-wide
   lock is on any wait, spawn or completion path.
6. **Priority is who waits.** A node's priority is a band and, within it, its slack against the
   request's critical path, from costs learned per code identity and kept across builds:

   | band | what |
   |---|---|
   | 0 | a running program or a person is blocked: a called function with no code yet, the diagnostics of the file just saved, a resumed fiber |
   | 1 | on the request's critical path |
   | 2 | the rest of the request |
   | 3 | tier promotion and speculation |
   | 4 | upkeep: eviction, store compaction, images |

   A waiter raises the owner of what it waits on to its own band. A job queued past a bound
   rises a band, so nothing starves (`bounded-progress`).
7. **Every scarce thing is admitted.** Cores, memory, IO threads and LLVM instances are budgets
   the scheduler grants. A node carries a predicted peak footprint, learned like its cost;
   admission is by footprint (`one-memory-measure`). A finished node's result is a record in the
   buffer pool, so memory in use is the work in flight plus what the pool chooses to keep, and
   the pool gives pages back under pressure by value per byte. A build that does not fit runs
   narrower, never past the ceiling.
8. **Tiers are nodes.** `code(f, tier 0)` (blaze's own fast backend) and `code(f, optimized)`
   (LLVM) are different nodes with different keys. A request's goal says which tier answers it:
   `edit` is done when every function the program can reach has tier 0 code, or, when running
   lazily, when the functions it has called do; `max` is done at the optimized tier. Under
   `edit` the optimized nodes still run, in band 3, hottest first by call counts, and each is
   patched into the binary and the running process as it lands. The status line always says
   the goal and how far promotion has come.
9. **An edit cancels and preempts.** New inputs mark the nodes that read them. A running node
   whose inputs changed stops at its next wait; its partial work is dropped, never published.
   The edit's own band 0 and 1 nodes go ahead of everything queued.
10. **Outputs never depend on the schedule** (`deterministic-output`). Order is restored by key
    where order matters. A node's record is the same bytes whichever thread, band or machine
    made it; the store check samples that.

## Hundreds of cores

The bound to design for is wall time near `max(T1 / P, T∞)`: total work over cores, or the
critical path, whichever is larger. What that needs, beyond the rules above:

- **Nothing serial per node.** Spawn, completion, a wait and a wake touch only the node's own
  atomics and one worker's deque. Counters are per worker and summed when read.
- **Queues per worker and per band, stolen nearest first**: same core cluster, then same NUMA
  node, then anywhere. A node's memory is first touched on the worker that runs it, and the
  buffer pool's frames are per NUMA node.
- **Batches sized to the machine.** With P cores a batch aims at a few milliseconds, so
  dispatch stays under 1% of work and a 200-core machine still has enough ready batches; the
  size comes from the predictor, and a batch that runs long splits and offers its tail.
- **A short critical path.** The serial stretches are the limit at high P: one crate's codegen
  as one task, the level loop, the layer barrier, the final link. Each becomes nodes (rules 1
  and 3); the link is the one true barrier, and its inputs are prepared in parallel.
- **The same graph across machines.** Because a node is a pure function of keyed inputs, a
  remote worker can run it against the shared store. That is how CI and a laptop borrow cores;
  it needs no second scheduler, only a transport.

The scaling test: one build at 16, 32, 64 and 128 vCPU in an ix VM (128 is the largest arm64 VM
today), wall time falling until `T∞`, zero contended waits on any scheduler lock, and the
simulator's prediction for 256 and 512 from the same transcript.

## How each kind of work enters

- **Lowering and name resolution**: one node per maintainer per crate, ready when its own
  dependencies' slots are set (the slots exist; name resolution's `layer()` goes).
- **Bodies**: one node per body for inference, borrow checking and lints, batched by module.
- **Effect summaries and facts**: one node per component of the call graph (rule 3), bottom-up,
  shared by every build script, crate and build that reaches the same instances.
- **Codegen**: a worklist. Describing an instance spawns the instances it reaches; a compiled
  function is published the moment it is done, and a waiter waits on that function. Rounds and
  levels go.
- **Link and patch**: a node per program; its inputs are per-function records.
- **Tests**: a test is a node keyed by the code identity of what it runs, so an unchanged test
  is a store hit and a save reruns only the tests its change reaches.

## What it must show

| measure, from spans, per build | 2026-11-11 | target |
|---|---|---|
| cores busy while runnable work exists | 17 to 43 of 64 | over 90% of P until the critical path is all that is left |
| wall / max(T1/P, T∞) | not computable yet | under 1.3 at 64 and at 128 cores |
| CPU spent recomputing a key another thread is computing | at least 4,573 s (the summary herd) | 0 |
| thread time waiting on a lock in a node | 6,555 s on two locks | 0: no such locks |
| footprint against the ceiling | reached it and was stopped | never over; narrows instead |
| time from `run` to a program's first instruction, `edit` goal | not measured | the cost of the code it executes first, not of what it can reach |
| time a band 0 node waits behind lower bands | not measured | p99 under 1 ms |

Proof is by simulation, not review (`dst-not-review`): blaze-sim properties over random graphs with
bands, cycles and cancellations (no lost wakeup, no starvation within the aging bound, a
component run by exactly one task, every true cycle refused and nothing else, the same records
under every schedule), and the transcript simulator replaying a real profile under each step's
policy before the step is built.

## Order of building

Each step lands alone and quotes the same query before and after.

1. **Summaries as component nodes** (`hir-ty-pure`). Before: the herd, about 30% of CPU.
2. **The two codegen locks become keyed records.** Before: 6,555 thread-s.
3. **Codegen as a worklist with per-function flights and learned costs.** Before: the level loop
   and one task per crate up to 340 s.
4. **Name resolution per crate; units wait on their own crate.**
5. **Bands, slack and raising through waits** in the pool's core, with the simulator's
   prediction first.
6. **Admission by predicted footprint; results read through the pool.**
7. **Tiers**: tier 0 and optimized code as separate nodes, the `edit` and `max` goals, promotion
   in band 3, patching into a running process; then lazy compilation on first call.
8. **NUMA placement and the 128-core scaling test.**
9. **Delete what was replaced**: rounds, levels, layers, `cost: 1`.

[fibers](fibers.md) and [concurrency](concurrency.md) describe the pool and the wait graph this
runs on; they fold into this file as steps 5 and 8 change them.
