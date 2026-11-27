---
status: plan
summary: How one command uses every core and holds little memory: one work-stealing thread pool and no fibers; item-grain tasks that never block (a task that lacks a fact returns `Need`, parks, and resumes when the producers have merged; nothing starts over, D157, D161); no global interner; a frame per worker reset per item; records as the only thing that outlives a task; footprint as the one measure; a refused allocation as a typed failure of one job; determinism by sorting at record boundaries; then the pieces still to build.
date: 2026-11-17
---
# Parallelism on 128 cores, and on a Mac

## End state (architect, 2026-11-16)

Workload: hundreds of thousands of tasks of 10 us to 10 ms (an outline, a crate's names, a
signature, a body, an instance), some needing a record another task is writing. Measured today
(tempo, one thread, port27/tempo-front-1116/tempo-front-9.txt): names of 1,026 crates 29.5 s,
heap 2,309 MB live after them; 20,000 bodies of one crate 31.2 s (640 a second); peak 2,589 MB.

1. **One scheduler: bstd's `Threads` pool; threads, work stealing, no fibers** (task/threads.rs
   stands: a worker per core of every level, LIFO slot, Chase-Lev deque, injection per LLC
   group). A parked fiber costs a stack per wait (hundreds of thousands of them) and carries
   thread-bound values across it (rule 27). A task never waits and is sans-IO (bottom.md). A
   task that lacks a fact returns `Need`, parks, and resumes when the producers have merged;
   NOTHING STARTS OVER (D157, D161: one ready-set driver in blaze-engine). (a) PLAN FROM TEXT: what a task can be
   seen to need from its text and the outlines (a body's paths and the heads they name, a
   signature's types, a crate's dependencies' scopes) is asked and settled before its first
   entry, in phases over shared tables (outlines, names, signatures, the impl index, bodies:
   the shape Sorbet and Pyrefly get their throughput from), so the common case enters each
   task once. (b) A fact found only while running (a method on an inferred type, a const in a
   type, an opaque's hidden type) is claimed in the claim table: made inline by the same worker
   when its key is unclaimed (point 3), else parked on with the task's work kept (its frame
   stays its own while parked), resuming at the question, never at line one. Every kind reports
   entries per made; the target is 1.0.
2. **Grain.** One item a task: a file's outline, a module's names (a glob cycle one `Group`), a
   signature, a body, an instance. The driver hands out batches of about 1 ms of predicted cost
   (learned per code identity; text length the first time), the items of one file; each item
   keeps its own key, record and failure. Dispatch stays under 1%. THE COORDINATOR IS NOT ON
   THE HOT PATH: today one thread makes every task's job and steps the machine once a batch
   (g044908: `jobs made` 33.2 s, steps 13.6 s of a 152 s run; g080231: 26.9 s and 13.0 s, puts
   7.1 s, of 168 s). A worker makes its own task's job; batches are planned in bulk and results
   merged in bulk, sorted at the boundary (point 8), so the serial part is a small constant
   per phase, not a step per task.
3. **Waiting without a graph.** A record being written has a claim: a slot in a fixed lock-free
   table by key (`blaze_core::claims`): empty, claimed by a task, or done with its address. A
   task needing key k claims it and runs it inline if free (help first: no restart), reads it if
   done, else parks: it joins k's waiter list keeping
   its state, and the publisher pushes the waiters onto its own deque. Each slot knows its owner and each parked task its one slot;
   before parking a task walks owner, owner's wait, ... (at most `live` steps): reaching itself
   is a cycle, found at the edge that closes it (SeqCst: of two tasks closing one cycle at once,
   one sees it). A module-scope cycle becomes one `Group`; any other is `Failure::MadeCycle`
   naming the items. The watchdog names a claim past its kind's deadline (`Hung`, rule 18).
4. **Sharing.** Only immutable bytes: records read in place (mapped, or a fresh record in its
   writer's per-worker segment buffer, published by the claim's Release store) and the claim
   table; counters per worker. **No global interner**: each task interns into a table in its
   frame; a record carries its own canonical table in the first-use order its key fixes; a
   reader re-interns what it touches. No shard lock, no id counter, no stop-the-world.
   NOTHING PRIVATE PER JOB: every global fact a body reads (names, heads and signatures, the
   impl index, lang items) is an immutable record read in place by every worker (impl buckets
   on blaze-surface/blaze-impls records, lane BK; heads frozen, piece 9); a body task's own
   state is only its inference tables, in its frame. Today each file job builds a private
   world per owner and makes again every head it reads (blaze-cli body_types.rs `Checker`,
   `FileTypes::new`). rustc's parallel front end scales poorly past 16 threads on shared
   mutable state: that is not rebuilt here.
5. **Memory by lifetime.** A frame per worker, reset per item; reserve is address space
   (`FRAME_RESERVE`, 4 GiB), committed pages are footprint. What outlives an item leaves as a
   record, never a pointer into a frame. The command's frame holds the request's rows and what
   the answer prints, nothing per task (a job names its file by index into the request's file
   table). Footprint = workers x largest working set in flight + the rows; mapped records are
   clean pages. Tempo keeps every crate's solved names live today (2.3 GB); here a crate's solve
   leaves as its `CrateScope` record. Estimate: under 0.5 GB private on 8 workers, about 4 GB on 128.
6. **One measure, one ceiling.** Footprint (phys_footprint; on Linux the cgroup's charge) per
   task (its frame's peak) and per command; the kernel's limit is the ceiling; nothing admits,
   throttles or narrows by memory (rule 50).
7. **A refused allocation fails one job, typed**: `FrameFull { task, frame, asked, peak,
   reserve }`; the build goes on. Closed by a type: blaze-own's growing methods that cannot fail
   (`push`, `reserve`, `with_capacity_in`) need `A: NeverRefuses` (global heap, `Kept`); a frame
   is not one, so only `try_*` and `?`. Until the bound lands, the lint `blaze::infallible_on_frame`
   denies them and the job seam reads the refusal from a slot the alloc hook fills, never a panic's text.
8. **Determinism.** A record's bytes are a function of its key and answers. Merges sort at the
   boundary: a unit's chunk and manifest by item key, findings by (item, ordinal), per-worker
   store batches by key at commit, gap lists by key (not by text). A test compares bytes of 1
   worker, 8, and reversed order.
9. **Sized by one rule.** Workers = cores of every level (Mac perflevel0 + perflevel1 at
   user-initiated; Linux the cgroup quota), started in proportion to queued work (a one-body edit
   stays on the main thread); frames and queues scale with workers, the claim table with the
   keys the request names (a power of two over twice that; 48 bytes a slot). Only that number differs between 8 cores and 128.
10. **Expected** (estimates; wall = T1/P + Tinf). Tempo cold front end at today's item costs:
   T1 about 130 s (names 29.5, heads 5, about 60,000 own bodies at 640 a second 94); Tinf, the
   crate chain's names, assumed 3 s (1 s with a crate's modules in parallel): 8 cores about 17 s,
   16 about 9 s, 128 about 2 to 4 s (today about 130 s). blaze's workspace (198 crates, 32.8 s
   today, mostly the one-thread names run): about 5, 3 and 1 s. Floor (a body 14 us, the same
   chain at its floor): tempo under 1 s on 16 cores, about 0.2 s on 128.

| System | What blaze matches, or does better by design |
|---|---|
| rustc parallel front end | sharded interner locks, blocking query waits, a deadlock-detector thread; blaze: no shared interner, a cycle found at the edge that closes it |
| Sorbet | frozen global state, files checked in parallel, merged on one thread; blaze merges by sorting per unit and runs names in parallel too |
| Hack, Flow | a lock-free shared hash heap over forked workers; blaze's mapped records are that heap, with threads and no heap collector |
| TypeScript's Go port | N checkers with their own types, some files checked twice; blaze: tables per task, each body once (its claim) |
| Zig | InternPool shards per thread under one index; blaze needs no index: records are keyed by content |
| mold | sizes first, then every thread writes at computed offsets; blaze writes segments and manifests so |
| Tokio, Rayon | LIFO slot, bounded queues, stealing, helping joins: bstd's pool; here no task blocks a worker |
| Fiber job systems (Go, Naughty Dog) | a stack parked per wait; blaze parks a restartable item with no stack |
| Roc, Carbon | an arena per module or check unit, canonical exposed types; blaze the same at item grain |
| salsa | blocking waits, cycles from an in-memory graph; blaze keeps no graph |

### Pieces still to build

The engine's ready-set driver (D157) and D161's claimed facts replace the old command's re-entry,
frozen heads with an overlay, heads by claim, bodies on the pool and jobs made on the worker.
Beside them:

- **Command frame**: the root frame reserves `FRAME_RESERVE`; a job names its file by index;
  growth through `try_`. Test: a check of a few thousand generated files under a small command
  frame finishes; with the copy back it fails typed, not by panic.
- **Typed refusal at the job seam** (`FrameFull`): the hook writes the refused layout into the
  running job's slot. blaze-mem's frame table already refuses with the frame, the bytes, the peak
  and the reserve (`crates/blaze-mem/src/frame/table.rs`); the seam adds the task. Test: a job
  planted to over-ask its frame fails with `FrameFull` naming the bytes; the scope goes on.
- **`Par` bounded**: `fn map<T: Send, F: Fn(usize) -> T + Sync>` (`crates/blaze-scope/src/task/fill.rs`
  has no bound). Test: the threaded run equals `Serial` and `Reversed` bytes.
- **Frame per body**: inference tables and round vectors in the worker's frame; gaps sorted by
  key. Test: a counting allocator sees zero global allocations per checked body.
- **`NeverRefuses`** and the lint on infallible growth over a frame. Test: lint UI cases.
- **Footprint per task**: each task's frame peak and the process footprint at batch end into the
  profile.
- **The claim table** (`blaze_core::claim_table`: claims, waiter lists, the cycle walk) is built
  and used by blaze-ask and front2. **The watchdog over claims** (`blaze_core::claim_watch`, a
  claim past its kind's deadline named as `Hung`) is built, not wired.
