---
status: plan
summary: Why the scheduler, not the thread pool, was the bottleneck in blaze v2 builds (barriers hiding work, unit costs of 1), and the plan that replaces it: real plane costs, write-once slots, per-crate maintainer nodes, name resolution per crate.
date: 2026-11-02
---
# v2-sched-design: is the scheduler the problem, and what replaces it

Lane: sched-design. Date: 2026-11-02. Measure and design only: no tree changes.
Tree read: /Volumes/Projects/blaze-v2 at `1d96f77a9f` plus the shared tree's uncommitted edits as of
13:30 (the sched-qos and impls-par fixes are in it). Span numbers come from the morning's parquet
profiles in /tmp/blaze-lanes/selfbuild-prof/data/{hf,self}, re-queried here (queries:
/tmp/sched-design-q{1..7}.py). Those runs were on the 18-CPU Mac at load average 93-176, with
12 workers at `utility` QoS. **The quiet 32-CPU VM run (/tmp/blaze-lanes/v2-selfbuild-vm.md) had
not landed when this was written**, so nothing below includes it (see "Open").

## Answer first

**Yes, the scheduler is the problem. But the thread pool is fine. The scheduling model is what's
wrong.** The pool's parts are sound: work-stealing deques, fibers that free their thread on a
wait, one wait graph that refuses cycles. They match Tokio, Rayon and rustc's fiber proposal. What
is wrong is what the scheduler gets to see and decide:

1. **Most of the build is hidden from the scheduler, behind barriers.** The plane schedules a few
   coarse nodes, every one with `cost: 1`. The real work runs inside them as bulk-synchronous
   phases:
   - name resolution runs one crate-graph layer at a time, each layer followed by a serial
     `&mut Db` apply;
   - lowering must finish for the whole workspace before any unit node exists;
   - codegen walks the call graph one level at a time;
   - codegen claims land only when a whole round ends.

   So workers sat **parked with nothing to run for 69.5% of worker time** in the hyperfine cold
   build (6,978 of 10,044 worker-seconds), and 48-55% in the self-builds. During name resolution
   only 13% (hf) to 24-33% (self) of the 18 workers were busy. The sum of each step's slowest
   crate was 86-98% of name resolution's wall time: the chain of barriers *is* the critical path.
   LLVM, the actual codegen, ran only in the last 37 s of the 558 s build.
2. **There is no priority below the plane.** Inside a node, jobs are FIFO/LIFO. `critical` is
   only a placement hint: stealing ignores it, and on a Mac, where every core now counts as
   fast, it does nothing. A wait does not raise the priority of the job it waits on. That is how
   one `utility` worker carrying the serial `impls` job became 46-65% of the critical path.
3. **Every contended wait in the process goes through one mutex**, `GLOBAL: Mutex<Global>`. Each
   run waited on it 2,288-4,404 times, for 31-61 s in total: a mean of 10-27 ms and a p99 of
   170-586 ms, for a critical section of a few dozen instructions. The holder gets descheduled
   while everyone else queues.

The morning's six findings split this way:
- **Symptoms of the model** (items 1-3 above): the codegen rounds, the `summary_in` pile-up, the
  serial `impls` on the critical path, and the scheduler-lock waits.
- **Real local bugs**, now fixed: the QoS class, the `impls` register opened per crate, and the
  watchdog.
- **A separate policy gap:** the daemon dropping its lowered database. Memory is not a resource
  the scheduler decides about.
- **A profiler gap:** the uncovered time.

**The top 3 changes, ranked by expected wall time saved.** The figures are for the hyperfine cold
build (558 s, under load); expect them to shrink in proportion on a quiet host.

1. **Dataflow lowering, with units pipelined behind it** (about 100-180 s of 558 s).
   - Each maintainer's work becomes one node per crate that starts when its dependencies'
     results are published, not when its layer is.
   - On a cold pass, a crate's result goes into a write-once slot, so publishing it cancels no
     reader.
   - Each unit waits for its own crate's publication, not for the whole workspace, so std and
     build-script codegen overlap lowering.
   - What it attacks: 0-220 s of hf b1 had 10-16 of 18 workers idle, about 2,700 idle
     worker-seconds.
2. **Per-function single flight, and dataflow walks, in codegen and the build-script proof**
   (about 100-200 s).
   - A compiled function lands at once, and a waiter waits on that function, not on another
     build's whole round.
   - The mono walk becomes a worklist, not a level-by-level loop.
   - The proof's `summary_in` walk becomes strongly-connected-component (SCC) tasks, shared by
     every build script.
   - What it attacks: 274-508 s of hf b1 had 10-16 workers idle. That window held 323.5 s of
     `codegen:` round waits, 134.7 s of round holds, and 1,789 s of `summary_in` waits summed
     over 16 concurrent units.
3. **Memory as a scheduled resource: keep what is expensive to recompute** (146 s per warm
   rebuild after idle).
   - Eviction becomes a background job that weighs recompute cost against bytes, never a side
     effect of opening a view or settling onto an image.
   - It records why each thing went.
   - What it attacks: hf b5 lowered again for 146.4 s, 35 s after a 1.09 s no-op.

Then two enablers, which pay off once (1) and (2) expose more parallel work:
- priority bands carrying a learned critical-path length (bottom level), raised through waits;
- a sharded wait graph that replaces `GLOBAL`.

## 1. Diagnosis: the mechanisms, with code and spans

### What the morning's findings were

| finding | mechanism | local bug or model? | status |
|---|---|---|---|
| 12 of 18 workers at `utility` QoS | `Topology::from_perflevels` marked P-cores slow; `bind_to` gave them `utility` | local bug | fixed by sched-qos: every worker is `user-initiated` (`pool.rs:1675`, `topology.rs:140`) |
| … and the critical job landed on one of them | `critical` is placement only, stealing ignores it, and no wait raises its owner's priority | **model (M2)** | open |
| serial `impls publish`, 46-65% of the critical path | one `register` per crate opened ~380 views, and each view swept every LRU table inline; it ran on one starved worker | local bug, plus **model (M1, M5)** | the register is fixed by impls-par (serial part now 0.7-2.2 s); the inline sweep is still the pattern |
| `build script proof` waited 1,789 s on `summary_in` | 16 recursive walks over std's instances, single-flighted on 11 owners, each a deep sequential recursion | **model (M4)** | open |
| codegen rounds hold each other up | a claim lands only when its whole round ends; a waiter parks on the round, not on the function | **model (M4)** | open |
| the daemon drops its lowered database | eviction and image settling decide memory with no cost model and no recorded reason | **model (M5)** | open |
| up to 57% of worker time under no span | driver time, the search, the park and the fiber switch have no span; open spans are lost at a stall | profiler gap (M6) | open |

### M1. Work is hidden behind barriers inside coarse plane nodes

- **The plane sees a handful of nodes, each at unit cost.**
  - `job.rs:502-503`: every node is `NodeRef { .., cost: 1, .. }`.
  - Lowering is one node: `job.rs:2504-2505` makes `Does::Lower` a `lone(.., NodeKind::Names, ..)`.
  - Units are added only after it, as phase nodes (`job.rs:2725-2800`). A unit's first phase
    waits for the last phase of each unit it depends on.
  - The plane's critical path (`plane.rs:1576-1588`, cost plus the path after) therefore ranks by
    node count, over nodes that each hide minutes of work.
- **Maintenance is bulk-synchronous on `&mut Db`.**
  - `Db::maintain` (`base-db/src/maintain.rs:537`) runs each maintainer in order, inside the one
    `lower workspace` task.
  - `run_maintainer` (`:571`) alternates parallel reads (`reads.run`, `:579`, which is
    `db.run_every` at `:161`, a barrier) with a serial apply on `&mut Db` (`:585`).
  - Name resolution runs two steps per crate-graph layer (`hir-def/src/nameres/delta.rs:112-142`,
    `layer()` at `:161`): a crate in layer k+1 waits for **every** crate of layer k, not only for
    its own dependencies.
  - The `impls` maintainer (order 200) starts only after name resolution has finished every crate
    (`hir-ty/src/method_resolution/delta.rs:84-110`).
  - Inside one crate, name resolution has its own barriers again: `decls_ahead`
    (`nameres/delta/input.rs:240-247`) runs `run_all` one module-tree depth at a time.
  - It is a write, so no other reader can run beside it: a write stops readers
    (`blaze_query::unwind_if_cancelled`, `Cancelled::PendingWrite`).
- **Codegen is level-synchronous too.** The instance walk does `graph.level(..)`, then
  `par::map(&level.functions, describe)`, then the next level (`blaze-codegen/src/driver.rs:1336-1350`).
  Each level waits for its slowest describe.

**Spans** (hf b1 = hyperfine cold, 558 s; self b1/b3 = self-build attempts; 18 workers):

| run | worker time parked with nothing to run (from `switches` idle→woke) | name resolution reads: wall | busy share of 18 workers | Σ slowest crate per step / wall | serial apply |
|---|---|---|---|---|---|
| hf b1 | **6,978 s of 10,044 (69.5%)** | 142.4 s over 324 steps | 13.2% | 122.4 / 142.4 s (86%) | 12.4 s |
| self b1 | 2,715 s (48.5%) | 106.3 s over 1,550 steps | 24.4% | 105.2 / 106.3 s (99%) | 20.8 s |
| self b3 | 2,996 s (54.8%) | 133.1 s over 1,627 steps | 33.5% | 130.8 / 133.1 s (98%) | 64.5 s |

The hf b1 timeline: idle workers per 30 s bucket, and the phase running in it.

| time | idle workers of 18 | phase |
|---|---|---|
| 0-220 s | 9-17 | `lower workspace`; no unit span exists before 220.9 s |
| 220-510 s | 10-16 | std units, then the 16 build-script units in `unit names` / `build script proof`, waiting |
| 510-558 s | 3-5 | `llvm compile` (12,372 spans, 518.6-555.0 s): the only stretch where the machine was full |

- One name-resolution step took 39.0 s.
  - Inside it, a single crate's task waited 35.7 s on the nested `run_all` of
    `nameres/delta/input.rs:245`.
  - That `run_all` covered 33 tasks on 3 threads, one of them 35.7 s.
  - The other 17 workers had nothing to run. That is the barrier, nested twice.
- Self-build b1 ran **1,486 `maintain` passes**, about one step each, inside one lowering. Each
  pass is its own write and its own set of barriers. The writes are not batched.

**The bound this costs.** The hf b1 workers were busy for about 3,065 worker-seconds. Packed onto
18 workers, that is about 170 s of wall. They took 558 s. Scheduling, not CPU, owns most of the
difference. A real schedule also has a critical path (T∞), so the true floor is
`max(T1/P, T∞)`. The barriers *add* to T∞: every layer adds its slowest crate, whether or not
anything depends on it.

### M2. There is no priority below the plane, and waits don't pass priority on

- **The pool has no priorities at all**, only FIFO deques, a LIFO slot and injectors
  (`pool.rs:1-30`).
  - `submit_critical` (`pool.rs:839`) only chooses a *fast group* to queue on.
  - The module doc says it plainly: "stealing is unchanged, so a slow worker with nothing else
    still takes it".
  - Since the QoS fix every group is fast on macOS (`topology.rs:140`), so `critical` does
    nothing on the Mac.
- **The scheduler core** (`sched/mod.rs`) does have per-core critical queues and a `Yield`, but
  the pool does not use them.
  - The pool joins the core only as a member with "its own placement" (`sched/host.rs:1-12`,
    `join_pool`).
  - So the core's placement policy never runs on the pool's jobs.
- **Nothing raises the priority of a job others wait on.**
  - A key's owner, and the `par` items of the scope it waits on, keep whatever queue position
    they had.
  - sched-qos says the same: "It would need priority inheritance … and the pool has none."
  - A woken waiter goes to the waker's LIFO slot (`host.rs:1-12`). That is good for the cache but
    knows nothing of priority.
- **The plane's critical path is computed over `cost: 1` nodes** (M1), even though `Predict`
  already learns a cost for each kind (`exec.rs:10`, `exec.rs:729`).

**Spans:**
- In self b1 the serial `impls` maintainer ran 126 s on blaze-work-9, a `utility` worker, at 46%
  of the critical path.
- In hf b5 the 146 s `lower workspace` ran on blaze-work-16.
- The QoS bug made this extreme. The mechanism remains: any long job a request waits on can sit
  behind, or be stolen onto, anything.

### M3. One process-wide lock for every wait

- `sched/fiber.rs:271-297`: `static GLOBAL: LazyLock<Mutex<Global>>`, "the process's one
  scheduler".
- What goes through it:
  - registering a job at its first wait or claim (`host.rs:413`);
  - every suspension (`host.rs:320`);
  - every `claim` of a waited key (`host.rs:514`, `:658`);
  - every `key_done` (`host.rs:445`, `:731`);
  - every scope open, release and close, which is every `par` loop (`host.rs:742-760`,
    `pool.rs:929-965`);
  - every registered job's end (`host.rs:413`);
  - `dispatch_global` under it each time.
- Uncontended key runs skip it (`pump.rs:13-16`). Scopes don't: every `par` loop takes it at
  least three times.

**Spans:** `wait` with `awaits = "lock the scheduler"`:

| run | waits | total | mean | p99 | max |
|---|---|---|---|---|---|
| hf b1 | 4,404 | 51.4 s | 11.7 ms | 274 ms | 1.60 s |
| self b1 | 3,111 | 31.7 s | 10.2 ms | 171 ms | 0.93 s |
| self b3 | 2,288 | 60.7 s | 26.5 ms | 586 ms | 1.62 s |

- A critical section of a few dozen instructions that takes milliseconds means the holder was
  descheduled while holding the lock. Under `utility` QoS that is priority inversion.
- The QoS fix shrinks it. The structure remains: one lock serialises the whole process's waits,
  and it gets worse with 32 or 64 cores.

### M4. Single flight at the wrong granularity

- **Codegen rounds** (`blaze-codegen/src/flight.rs`):
  - A build's claims between two releases are one round, held as one scheduler run
    (`begin`, `:226`).
  - The driver claims a whole batch of store misses, compiles all of its own, lands them all,
    and only then releases (`driver.rs:2591-2643`, `:2760-2845`).
  - A waiter for any one function parks on the round that claimed it (`flight.rs:112-147`). So it
    waits for that build's whole batch (thousands of functions), even when its function was done
    in the first second.
  - Then it releases its own claims and claims again: ping-pong between builds.
- **Spans (hf b1):** 54 `hold` spans totalling 134.7 s (longest 39.1 s, with 0.21 s of CPU), 24
  `codegen:` waits totalling 323.5 s.
- **The build-script proof** (`blaze-cargo/src/serve/unit/script/pure.rs:146`) calls
  `hir_ty_pure::script_purity`. That recurses through `summary_in` (`hir-ty-pure/src/walk.rs:359-365`)
  over every instance reachable from `main`: a depth-first recursion, sequential per root.
  - 16 build scripts walk mostly the same std instances.
  - Single flight makes 15 of them park on whichever of 11 owners got there first.
  - The owners get no priority (M2), and each recursion has no parallelism of its own.
- **Spans (hf b1):** 1,788.7 s of `summary_in` waits over 70 waits, longest 166.6 s.
  - That is summed over concurrent waiters, not wall time.
  - The wall effect is the 274-458 s `build script proof` window, with 10-16 workers idle.

### M5. Memory is decided outside the scheduler, with no cost model

- **Every view sweeps every LRU table inline.**
  - `Storage::new_view` calls `sweep` (`blaze-query/src/db/function.rs:1245`) and then
    `sweep_by_frequency` (`:1260`), which hashes and drops values over the cap on the thread that
    opened the view.
  - impls-par traced core's 2.17 s publish stall to exactly this: one `lru evict` of
    `parse_macro_expansion`.
  - It is now parallel past 256 values, but still inline, on the writer, at whatever moment a
    write happens.
- **The daemon's shed.**
  - `images.step` runs on every loop turn (`serve/daemon.rs:1160-1163`). After a cold build the
    database settles onto the mapped image (`serve/image.rs`; `Database::settle` swaps memos to
    `Stored::Disk`).
  - serve.log showed query memory falling from 3.63 to 0.47 GiB within about 35 s, and the next
    build lowered for 146.4 s instead of 0.02 s.
  - The image then had 10 sections and lacked "the library syntax trees 59.2 MiB".
  - **What exactly was dropped, and why, is not known.** Neither this lane nor selfbuild-prof
    found it, and no event says. That missing reason is the design point.
- **The plane's memory admission** (`plane.rs:28-40`) knows predicted peaks. Nothing uses
  recompute cost to decide what stays.

### M6. The profiler can't see the scheduler

- Uncovered worker time reached 47.9% (self b1) and 57.5% (hf b5, blaze-work-16).
- Driver time (neither job nor idle) was 48-66 s per worker in self b3, 16-22% of wall.
- `lock the scheduler` names no owner.
- `self_cpu_ns` undercounts by about 3x (selfbuild-prof).
- The fast workers each started about 23k jobs and parked about 19k times: tiny jobs, with
  park/unpark churn.

### What is *not* the problem

- **Fibers can migrate.** A parked job's record goes back to the pool's queues and runs on
  whichever worker takes it (`host.rs:269-290`). Thread-locals are carried (`fiber.rs` doc).
- **Waits mostly don't pin workers.** Pumping is gone, and a key wait or a scope wait suspends the
  fiber (`host.rs:300-345`, `pool.rs:944-958`). The exceptions:
  - the `GLOBAL` mutex, which is a real thread block;
  - the scope's final `SpinWait` (`pool.rs:953`), which is short;
  - the `&mut Db` write, which stops every reader of the database.
- **Work-stealing itself is fine.** Chase-Lev deques, a LIFO slot, half-steals and Go/Tokio
  wake rules (`pool.rs:1-30`) are what Tokio and Rayon do. They are throughput-fair by design,
  and lack only priorities (M2).

## 2. How others schedule this kind of work, and what fits blaze

| system | unit the scheduler sees | wait model | priority | lesson for blaze |
|---|---|---|---|---|
| **Buck2 / DICE** ([modern DICE](https://buck2.build/docs/insights_and_knowledge/modern_dice/), [slides](https://buck2.build/assets/Modern_DICE.pdf)) | every key is a Tokio task, spawned once per transaction | `await` on the key's shared future | none beyond Tokio | (a) **every key is schedulable work**, with no phases; (b) its state moved to **one single-threaded core-state owner, fed by messages**, after fine-grained locks "became complex", which is blaze's plane owner thread; (c) `join_all` records dependencies as a series-parallel graph, so re-checking is ordered where it must be and parallel elsewhere |
| **Bazel Skyframe** ([Skyframe](https://bazel.build/reference/skyframe), [StateMachines](https://bazel.build/contribute/statemachine-guide)) | every SkyKey | no blocking: a missing dependency returns `null` and the function **restarts**; `SkyKeyComputeState`/StateMachines remove the re-done work | evaluator queue | dependencies are declared per key (`getValues` evaluates them in parallel), never per phase. Blaze's fibers are the better wait model (no restarts); take the **per-key dependency declaration** |
| **salsa** ([cycles and blocking](https://salsa-rs.github.io/salsa/plumbing/cycles.html), [runtime](https://salsa-rs.github.io/salsa/plumbing/database_and_runtime.html)) | a query on a cloned handle | the thread blocks; the `DependencyGraph` is behind **one mutex**, checked for cycles before blocking | none | the same shape as blaze's `GLOBAL`. Salsa's waiters are threads (few), blaze's are jobs (many), so blaze must **shard** it. A write cancels every handle (`cancel_others`): the stop-the-world that forces blaze's maintainers into barriers |
| **rustc parallel front end** ([blog 2023](https://blog.rust-lang.org/2023/11/09/parallel-rustc/), [Zoxc's design](https://internals.rust-lang.org/t/parallelizing-rustc-using-rayon/6606)) | Rayon jobs at a few `par_*` points | the original design used fibers per stolen job; it shipped Rayon with a **deadlock handler**: cycles found lazily, only when the pool goes idle | none | parallelism added at fixed loop points gives "patchy" utilisation (the blog's own word), and up to 50% with 8 threads. Blaze's `par` points are the same situation. **Lazy cycle detection costs nothing until the pool idles**, a fit for M3 |
| **Tokio / Rayon** | tasks / join closures | the task yields, or the thread helps while it waits | none (Tokio has a LIFO slot and a budget) | blaze already copies their mechanics. They don't, and needn't, know a critical path; blaze's builds do |
| **Shake** ([pool](https://hackage.haskell.org/package/shake-0.19.5/docs/src/General.Pool.html), [10,000 ft](http://neilmitchell.blogspot.com/2019/06/shake-from-10000ft.html)) | every rule; continuations | an `Action` blocked on a `Running` releases its thread | **bands: Exception > Resume > Start > Batch > Deprioritize** | **resumed work goes ahead of new starts**: it holds memory and others wait on it. A "Batch" band for work that gets cheaper when it waits. Bands, not a total order |
| **Critical-path list scheduling** ([Kwok and Ahmad survey](http://charm.cs.uiuc.edu/users/arya/docs/6.pdf), HLFET; Blumofe-Leiserson T1/P + O(T∞)) | DAG nodes with estimated costs | n/a | the **bottom level**: the longest path of cost from the node to the end | priority = bottom level from learned costs. And the bound says wall ≥ T∞, which barriers inflate, so priority cannot fix M1; only removing barriers can |
| **Priority work stealing** ([Wimmer et al. PPoPP'14](https://www.cse.chalmers.se/~tsigas/papers/PPoPP14.pdf), [Imam-Sarkar](https://github.com/shamsimam/priorityworkstealing)) | tasks with a fixed priority | n/a | **per-worker priority queues with relaxed global order**; steal from the highest band | fits blaze's pool: a few bands per worker, no global heap |

GitHub search (repositories pushed since 2025 about priority, critical-path, task-graph or fiber
schedulers) found nothing beyond small thread pools. The relevant prior art is the systems above.

**What fits blaze.** Keep the mechanics: fibers, stealing, one wait graph, a sans-IO core. Change
the model:
- like DICE, Skyframe and Shake, **every piece of work is a node with per-key dependencies, and
  there are no phases**;
- like Shake and the list-scheduling literature, **bands plus a bottom level**, with resumes
  first;
- like rustc and salsa, **cycle detection off the hot path**, sharded instead of one mutex;
- unlike salsa, **publication without stop-the-world** on a cold pass.

## 3. The design

### 3.1 Principles

1. **One graph.** Everything a build does is a node the scheduler sees, with the dependencies it
   really has: per crate per maintainer, per unit phase, per batch of codegen functions. A
   barrier is allowed only where the semantics really are all-of (a link, the closed program of
   a release build), and it is named as a node.
2. **Priority = band, then bottom level, raised through waits.** Costs are learned from spans,
   never `1`.
3. **Waits are per key and sharded.** No process-wide lock on the wait path.
4. **Memory is a scheduler resource.** What stays is decided by recompute cost per byte, by a
   scheduled job, with each decision's reason recorded.
5. **Sans-IO** (rule 12). Every decision above is made in a synchronous core, from transcripts,
   with an injected clock. The pool and fibers are drivers.

### 3.2 How work enters: maintainers, lowering and codegen

**Write-once publication slots: the enabler.**
- A maintainer's output for crate C becomes a set of **write-once slots** keyed `(maintainer, C)`
  in blaze-query, alongside inputs.
- A query that reads an unset slot **waits on its key** (the existing `pump::Flights` wait, which
  suspends the fiber).
- Setting a slot that no reader has seen takes `&Db`, not `&mut Db`. Nothing can be invalid,
  because every reader waited, so it opens no view and cancels no reader.
- Setting a slot that *has* been read (an edit) is an ordinary input write. Those are
  group-committed as today: one view per edit.
- Purity holds. A memo records the slot as a dependency, like an input. Before it is set, a
  reader can only wait.
- Rule 18: a second, different write to a write-once slot is a typed refusal naming the
  maintainer, the crate and the slot.

**Maintainers as per-crate nodes.**
- `Maintainer { reads, apply }` becomes `Maintainer { deps(C) -> keys, run(&Db, C, &mut state_C) -> Publish }`.
- The scheduler adds one node per (maintainer, crate), ready when its dependency keys are set.
- The serial part shrinks to the per-crate publish into slots. It needs no global order, because
  slots are keyed. Rule 22: ids are given per crate, as impls-par already does by replaying ops
  in crate order.
- The `impls` maintainer is the simplest first port: crate C's tables need only C and its
  dependencies (orphan rules). Name resolution follows; its `layer()` barrier disappears.
- `decls_ahead`'s depth loop becomes a worklist that spawns children as it discovers them.
- On an edit (slots already read), the same per-crate nodes run, and their publishes are batched
  into one write, so incremental cost stays what DBSP gives today.

**Units pipelined behind lowering.**
- `Does::Lower` stops being one node. Unit phase nodes are added at plan time.
- Each unit's `Names` phase depends on its crate's publication keys (and its dependencies'), not
  on the workspace.
- std's units depend only on the library's lowering, which comes from the image, so std
  codegen starts in the first seconds.
- Build scripts compile as soon as their crate and std are ready, as under cargo.

**Codegen.**
- **Per-function flights.**
  - `Claims::land` is called per function, or per batch of at most 64 as each finishes
    (`until_stopped` already yields per item).
  - A waiter parks on the *function's* flight key, not on the round.
  - Rounds go. A build releases a claim as soon as it has landed or given up on it.
  - The ping-pong goes too: a waiter waits only for the functions it needs.
- **The mono walk as a worklist.**
  - `describe(instance)` is a job that spawns its newly discovered callees (dedup through a
    papaya set).
  - Rule 22: the program's order is restored by sorting at the end, not by levels.
  - The level limit becomes a depth carried with each instance.
- **The build-script proof.**
  - `summary_in` becomes a bottom-up pass over the strongly connected components (SCCs) of the
    reached call graph.
  - Each SCC is a task, ready when its callees' SCCs are done.
  - It is keyed by instance, so the 16 scripts share std's SCCs. A waiter waits on one SCC's
    key, and the owner of that key inherits the waiter's band.

### 3.3 Priorities

- A job's priority is `(band, bottom_level)`.
- Bands, highest first:

  | band | what |
  |---|---|
  | 0 | resumed or raised work: a job some waiter needs (Shake's Resume) |
  | 1 | foreground and on the request's critical path: bottom level within 10% of the request's critical-path length |
  | 2 | foreground, other |
  | 3 | idle-class requests (the plane's existing class), speculation |
  | 4 | upkeep: eviction, images, store writes that nothing waits for |

- **Bottom level.**
  - The plane computes it, as it does now (`plane.rs:1576-1588`), but over
    `Predict::predict(kind, key)` costs instead of `cost: 1`.
  - The predictor's cost comes from span time per (kind, crate), kept in the daemon image, as
    `Estimates` already is.
  - A sub-job (a `par` item, a scope job, a maintainer's crate node) inherits its spawner's
    priority.
- **Raising through waits.** When job W suspends on key K, whose owner is O, or on counter S:
  - O's priority becomes `max(O, W)`, and so do the queued items of S;
  - it is undone when the wait ends (the next spawn recomputes it).
  - The wait graph already knows these edges (`sched/graph.rs`). Raising one is a relaxed store
    of the band into the job's slot; a queued job is re-queued at its next pick.
- **Queues.**
  - Each worker has one Chase-Lev deque per band (5) plus the LIFO slot. The owner pops the
    highest non-empty band.
  - A thief scans bands from highest, and within a band steals nearest first (today's tiers).
  - That is relaxed priority (Wimmer, Imam-Sarkar): no global heap, and no ordering promise
    beyond "no lower band runs on a worker while a higher band is queued anywhere it could
    steal".
- **Aging** (rule 15). A band-3 or band-4 job queued longer than a bound from the injected clock
  moves up one band, so nothing starves.

### 3.4 The wait model

- **Keep fibers.** A wait frees its thread; a resume runs on any worker. Keep `NoWait`, `Carried`
  and the cycle refusal ("a wait that would close a cycle is refused").
- **Shard the wait graph.**
  - Each job slot gets an atomic `waits_on`: a job, a key flight or a counter. Each key flight
    and counter has its own small lock (the `pump` table's bucket latch already exists).
  - A wait first publishes its own `waits_on`, then walks owner, then owner's `waits_on`, and so
    on, on atomics. The walk is bounded by a depth from the job count (rule 15).
  - If the walk reaches the asker, the wait is refused.
  - Two waits that close one cycle at the same time can both see it and both be refused. That is
    safe, because a refused wait re-runs, and it is rare; the model checker pins it.
  - A counter with many items is the one case that keeps a lock: its own, not a global one.
- **Lazy backstop.**
  - When every worker is parked while jobs wait (`check_stall`, already there), run the full
    cycle search once: rustc's deadlock handler.
  - It is a check that the sharded walk missed nothing. Under `blaze_check` and in tests it
    fails loudly; in a build it is a typed error naming the cycle, never a silent retry.
- **`GLOBAL` goes.**
  - Placement and the timer wheel stay sans-IO in `Sched`, as pure functions over snapshots of
    the atomics (as `sched.md` phase 2 planned at 20:40, "the run queues move to the driver").
  - Spawn, end and an uncontended scope take no lock.

### 3.5 Memory and eviction

- **Eviction is a band-4 job, never part of opening a view.**
  - `new_view` only marks the tables over cap; the sweep runs as a scheduled job.
  - It runs at once when admission is over budget, otherwise when a worker would park.
  - Admission (`plane.rs` Budget) holds back new tasks before it evicts expensive state.
- **What stays is decided by value per byte**: `recompute_cost × P(reuse) / bytes`.
  - `recompute_cost` is the query kind's span self-CPU, plus its inputs' if they would go too.
  - `P(reuse)`: the W-TinyLFU frequency this assumed is gone (lane lrucap, 2026-11-02:
    recency by the row's stamp, values kept by bytes, `blaze_query::db::budget`).
  - Cheap-to-recompute values (parse trees) go first; a lowered workspace's def maps and impl
    tables go last.
- **A live checkout's lowered front end is pinned while under budget.** An idle daemon does not
  shed it. Settling onto an image must keep it reachable; rule 23 already asks that a restored
  state equal a fresh build.
- **Every eviction and every shed emits an event** naming the table, the bytes, the estimated
  recompute cost and the reason (budget, idle, image), in the profile's events table. The
  146 s re-lower would then have said why.

### 3.6 Sans-IO and testable (rule 12)

- **The core** is `sched::Sched`, extended. It reads no clock, starts no thread and does no IO.
  - New inputs: `spawn(job, Spawn { band, bottom_level, parent, .. })`,
    `suspended(job, Wait)` (it raises the owner), `done`, `key_done`, `set_priority`,
    `memory(held, budget)`, `advance_clock(now)`.
  - New outputs: `Run`, `Wake`, `Yield`, `Raise { job, band }`, `Evict { table, bytes }`, and
    `next_deadline` for aging.
  - The plane keeps request-level fair share and admission. The core takes over job-level order.
- **The drivers** (the pool on fibers, `exec::Scheduler`) do only switches, parks and
  `pthread`/QoS calls.
- **A transcript simulator.**
  - It turns a profile's spans and switches into a transcript: jobs, their cost (self CPU, or
    wall where CPU is missing), their key waits and scope counters, and their spawns.
  - It replays the transcript through `Sched` on P virtual cores with a virtual clock.
  - Calibration: replaying hf b1 under *today's* policy (one plane node for lowering,
    layer barriers) must reproduce the measured idle fraction (69.5%) within 5 points.
  - It then replays the same transcript under each migration step, which predicts each step's
    wall **before** it is built.
  - This is rule 12's "a test replays a transcript" and rule 20's cause test, done offline.

### 3.7 The tests and metrics that prove it

| metric (from spans, per build) | today | target |
|---|---|---|
| worker time parked while a runnable job exists anywhere ("idle with work") | not measured; total parked is 48-70% | < 2% |
| `T_wall / max(T1/P, T∞)`, with T∞ from a per-query critical path | not computable (the `critical_path` view stops at the job) | < 1.3 |
| barrier waste Σ(wall × P − busy) over named barriers | name resolution alone: about 2,200 worker-s (hf b1) | < 5% of worker time |
| `lock the scheduler` waits | 31-61 s, p99 170-586 ms | gone (no such lock) |
| priority inversion: time a band-0/1 job waits on an owner that is queued, not running | not measured | p99 < 1 ms |
| re-lowering after idle | 146.4 s (hf b5) | 0, and any eviction has a reason event |
| uncovered worker time | up to 57.5% | < 1% (docs/profiling.md's gate) |

Tests:
- **Unit tests in `sched/tests.rs`:**
  - `a_waited_on_job_runs_at_its_waiters_band`;
  - `a_higher_band_is_never_queued_while_a_worker_runs_a_lower_one_it_could_have_stolen`;
  - `an_aged_job_rises_one_band_per_bound`;
  - `eviction_drops_the_cheapest_value_per_byte_first`.
- **Hegel properties** over random DAGs with bands:
  - no lost wakeup;
  - no starvation within the aging bound;
  - every cycle refused, and nothing refused that closes no cycle;
  - the sharded walk agrees with the full search on every transcript.
- **Under `blaze_check`:** two waits closing one cycle at once are both refused, never both
  admitted.
- **base-db:** `a_crate_starts_when_its_dependencies_publish_not_its_layer`, on a fixture of a
  deep chain plus a wide layer, timed on the transcript clock.
- **blaze-query:** `a_first_write_of_a_slot_cancels_no_reader`, and
  `a_second_different_write_of_a_slot_is_refused_naming_it`.
- **Rule 23:** restore after an image settle, then a warm build runs no `lower workspace` work.
- **Scaling, in an ix VM:** the same self-build at 16, 32 and 64 vCPU. The wall should fall
  until T∞, and lock-free waits should stay at 0 contention.

## 4. Migration: each step lands alone, with a before and after from spans

Steps 0-1 are cheap and make every later claim checkable. Steps 2-4 carry most of the wall
time. Steps 5-7 make it scale and keep it.

| step | change | lands alone because | before → after (spans) |
|---|---|---|---|
| **0. See it** (rule 14) | spans for worker driver time (search, park, switch); the owner named on `lock the scheduler`; a `crate` field on maintainer per-crate tasks; an "idle with work" counter in `switches`; a per-query critical-path view; the transcript simulator, calibrated on hf b1 and the VM run | adds observation only | uncovered worker time 57% → < 1%; the simulator reproduces hf b1's 69.5% idle within 5 points |
| **1. Real costs in the plane** | `node()` takes `Predict`'s cost per (kind, key), not `1` (`job.rs:502`); costs learned from span time per (kind, crate) and kept in the image | the order changes only among nodes already ready | the plane's start order matches bottom level; the simulator's predicted wall for later steps |
| **2. Codegen per-function flights** | land per function or per ≤64-function batch; wait on the function's key; no rounds (`flight.rs`, `driver.rs:2591-2852`) | blaze-codegen only; the store keys are unchanged | `hold` 134.7 s and `codegen:` waits 323.5 s → about 0; the wall of the build-script `unit codegen` window |
| **3. Write-once slots plus the `impls` maintainer per crate** | the blaze-query slot kind (first write takes `&Db`, a second different write is refused); `impls` as per-crate nodes waiting on dependency keys | one maintainer; name resolution is unchanged | the `impls` maintainer's wall → its dependency-chain bound; views opened per cold lowering (impls-par: 1) stay 1 |
| **4. Name resolution per crate; units pipelined** | `layer()` and the two-steps-per-layer loop go; `decls_ahead` becomes a worklist; `Does::Lower` splits; unit `Names` nodes wait on their crate's publication keys | relies on 3; the incremental path still group-commits | name resolution busy share 13-33% → > 80%; Σ slowest-per-step stops being the wall; time to the first `llvm compile` 518 s → under 60 s (hf b1 shape); idle workers in the 0-220 s window 9-17 → < 2 |
| **5. Mono walk as a worklist; proof as SCC tasks** | `driver.rs:1336-1350` levels go; `summary_in` becomes bottom-up per SCC, shared across scripts | codegen and hir-ty-pure only | `summary_in` waits 1,789 s → per-SCC waits under 1 s each; `unit names` / `build script proof` wall |
| **6. Bands, bottom level and raising through waits in the pool** | per-band deques, steal highest band first, raise on wait, aging; decisions in `Sched`, the pool as a driver | the pool's API is unchanged (`submit_as` maps to a band) | the inversion metric's p99; `T_wall / max(T1/P, T∞)` |
| **7. Sharded wait graph, no `GLOBAL`** | atomic `waits_on` and the bounded walk; a per-counter lock; the lazy backstop at `check_stall` | semantics are kept (refuse cycles), and it is model-checked | `lock the scheduler` waits 31-61 s → 0; the scaling test at 32 and 64 vCPU |
| **8. Eviction as a scheduled job, with a value model** | no inline sweep in `new_view`; band-4 sweep jobs; value per byte; the live checkout's front end pinned under budget; reason events | the memory path only | hf b5's 146.4 s re-lower → 0; RSS within budget; every shed has an event |
| **9. Delete what the model replaced** | the round code, `layer()`, the level loop, and `sched::fiber::Driver` if the pool now runs on the core alone | after 2-8 | lines removed; no span names refer to rounds or layers |

Rules this keeps:
- **10.** Queues are crossbeam/Chase-Lev; the slots are papaya/atomics.
- **12.** Each new decision is in `Sched` or the plane, with replay tests.
- **13.** Every new wait names its key and owner. A slot wait names the maintainer and the crate.
- **14.** Each step quotes its before and after spans.
- **24.** The slot waits are ordinary fiber waits, with nothing held across them.
- **25.** It owns the scheduling layer instead of tuning its constants.

## Open

- **The quiet VM run** (/tmp/blaze-lanes/v2-selfbuild-vm.md) was not in when this was written.
  Re-run `/tmp/sched-design-q2.py` (barrier efficiency), `q6.py` (idle from `switches`) and
  `q7.py` (lock waits) on its profile.
  - **Expected:** less lock wait and less wall time, because the QoS fix is in and the host is
    quiet. The same shape: low name-resolution busy share, no unit before lowering ends, and
    LLVM bunched at the end.
  - **If the parked share falls below about 20% there**, the morning's idle was mostly load,
    and steps 4-5 drop in rank.
- **The give-way (`Cancelled`) rate** is not measured. A refused wait re-runs its task from the
  start. If it is frequent, the sharded walk's occasional double refusal matters more. Step 0
  should count it.
- **Why the daemon dropped the lowered database** (M5) is still unexplained. Step 8 does not
  depend on knowing it, but the reason events would have caught it.
- **Self b3's last 90 s**: all 18 workers were busy in `maintain` jobs and nothing completed,
  with `macros_writing_impls` waits at 211.7 s. That is either a starved CPU or a give-way loop.
  The VM run will tell which.
- **Every figure here is from a host at load 93-176**, and wall times are inflated. The
  idle-worker figures are not inflated by load: a parked worker had nothing queued. So the
  structural claim stands, and only the seconds-saved estimates scale.
